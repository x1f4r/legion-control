import assert from 'node:assert/strict';
import fs from 'node:fs';
import childProcess from 'node:child_process';
import { syncBuiltinESMExports } from 'node:module';
import test from 'node:test';
import { linuxSchedulerCommand, runDoctor, schedulerTimeLimitCheck, schedulerTimeLimitMs } from '../src/doctor.mjs';
import { loadConfig } from '../src/config.mjs';
import { withHome, writeConfig } from './helpers.mjs';

const base = '/home/me/.legion-control';
const node = '/home/me/.local/opt/node-v24.20.0-linux-arm64/bin/node';
const home = '/home/me';
const inspect = (command) => linuxSchedulerCommand(`[Service]\nExecStart=${command}\n`, { base, node, home });

test('doctor accepts the installed same-base stable launcher cycle', () => {
  assert.equal(inspect(`"${base}/bin/legionctl" cycle`).ok, true);
  assert.equal(inspect(`${base}/bin/legionctl cycle`).ok, true);
  const template = fs.readFileSync(new URL('../install/legion-control-update.service', import.meta.url), 'utf8');
  const escapedBase = '/home/user/Legion "test" \\ 100%';
  const launcher = `${escapedBase}/bin/legionctl`.replaceAll('\\', '\\\\').replaceAll('"', '\\"').replaceAll('%', '%%');
  assert.equal(linuxSchedulerCommand(template.replaceAll('@LAUNCHER@', launcher), { base: escapedBase, node }).ok, true);
});

test('doctor retains direct Node invocation of the same agent entry', () => {
  assert.equal(inspect(`${node} ${base}/agent/src/index.mjs cycle`).ok, true);
  assert.equal(inspect(`"${node}" "${base}/agent/src/index.mjs" cycle`).ok, true);
  assert.equal(inspect(`/unrelated/node ${base}/agent/src/index.mjs cycle`).ok, false);
});

test('doctor refuses another installation, unrelated commands, expansions and extra arguments', () => {
  for (const command of [
    `"/other/installation/bin/legionctl" cycle`,
    `"${base}/bin/legionctl-other" cycle`,
    `"${base}/bin/legionctl" update`,
    `"${base}/bin/legionctl" cycle --force`,
    `/bin/echo ${base}/agent/src/index.mjs cycle`,
    `/bin/sh -c '"${base}/bin/legionctl" cycle'`,
    `"${base}/bin/legionctl" cycle ; /bin/echo other`,
    `"${base}/bin/legionctl" cycle-other`,
    `"${node}" "/other/agent/src/index.mjs" cycle`,
    `"%h/.legion-control/bin/legionctl" cycle`,
    `"\u0024HOME/.legion-control/bin/legionctl" cycle`,
    `-"${base}/bin/legionctl" cycle`,
  ]) assert.equal(inspect(command).ok, false, command);
});

test('doctor follows ExecStart resets and refuses multiple active commands', () => {
  const valid = `ExecStart="${base}/bin/legionctl" cycle`;
  const invalid = 'ExecStart=/other/legionctl cycle';
  assert.equal(linuxSchedulerCommand(`[Service]\n${valid}\n${invalid}`, { base, node }).ok, false);
  assert.equal(linuxSchedulerCommand(`[Service]\n${valid}\n[Service]\nExecStart=\n${invalid}`, { base, node }).ok, false);
  assert.equal(linuxSchedulerCommand(`[Service]\n${invalid}\n[Service]\nExecStart=\n${valid}`, { base, node }).ok, true);
  assert.equal(linuxSchedulerCommand(`[Unit]\n${valid}`, { base, node }).ok, false);
  assert.equal(linuxSchedulerCommand(`[Service]\n# ${valid}`, { base, node }).ok, false);
});

test('legacy Node cannot be approved with a redirected or unproven home', () => {
  const legacy = `ExecStart="${node}" "${base}/agent/src/index.mjs" cycle`;
  const stable = `ExecStart="${base}/bin/legionctl" cycle`;
  for (const environment of [
    'Environment=LEGIONCTL_HOME=/other',
    'Environment="HOME=/other"',
    'Environment=PATH=/usr/bin "LEGIONCTL_HOME=/other"',
    'EnvironmentFile=/some/environment',
    'PassEnvironment=LEGIONCTL_HOME',
    'UnsetEnvironment=HOME',
  ]) {
    for (const unit of [`[Service]\n${environment}\n${legacy}`, `[Service]\n${legacy}\n[Service]\n${environment}`]) {
      assert.equal(linuxSchedulerCommand(unit, { base, node, home }).ok, false, environment);
    }
    assert.equal(linuxSchedulerCommand(`[Service]\n${environment}\n${stable}`, { base, node, home }).ok, true);
  }
  assert.equal(linuxSchedulerCommand(`[Service]\nEnvironment=PATH=/usr/bin\n${legacy}`, { base, node, home }).ok, true);
  const custom = '/srv/legion';
  assert.equal(linuxSchedulerCommand(`[Service]\nExecStart="${node}" "${custom}/agent/src/index.mjs" cycle`, { base: custom, node, home }).ok, false);
});

test('doctor reads how long each scheduler lets a cycle run', () => {
  const minutes = (value) => value * 60 * 1000;
  const unit = fs.readFileSync(new URL('../install/legion-control-update.service', import.meta.url), 'utf8');
  assert.equal(schedulerTimeLimitMs(unit, { kind: 'unit' }), minutes(30), 'the unit as installed');
  for (const [text, expected] of [
    ['TimeoutStartSec=900', minutes(15)],
    ['TimeoutStartSec=30min', minutes(30)],
    ['TimeoutStartSec=1h 30min', minutes(90)],
    ['TimeoutStartSec=infinity', Infinity],
    ['TimeoutStartSec=0', Infinity],
    ['TimeoutStartSec=900\nTimeoutStartSec=2400', minutes(40)],
    ['# TimeoutStartSec=900', null],
    ['TimeoutStartSec=soon', null],
    ['TimeoutStartSec=5 fortnights', null],
  ]) assert.equal(schedulerTimeLimitMs(text, { kind: 'unit' }), expected, text);

  const task = (limit) => `<Task><Settings><MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>${limit}</Settings></Task>`;
  for (const [limit, expected] of [
    ['<ExecutionTimeLimit>PT15M</ExecutionTimeLimit>', minutes(15)],
    ['<ExecutionTimeLimit>PT30M</ExecutionTimeLimit>', minutes(30)],
    ['<ExecutionTimeLimit>PT1H30M</ExecutionTimeLimit>', minutes(90)],
    ['<ExecutionTimeLimit>P1DT2H</ExecutionTimeLimit>', minutes(26 * 60)],
    ['<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>', Infinity],
    ['', minutes(72 * 60)],
    ['<ExecutionTimeLimit>PT</ExecutionTimeLimit>', null],
    ['<ExecutionTimeLimit>fifteen</ExecutionTimeLimit>', null],
  ]) assert.equal(schedulerTimeLimitMs(task(limit), { kind: 'task' }), expected, limit || 'no limit named');

  assert.equal(schedulerTimeLimitCheck(minutes(30), 'fix it').level, 'ok');
  assert.equal(schedulerTimeLimitCheck(Infinity, 'fix it').summary, 'the scheduler lets a cycle run as long as it needs');
  const short = schedulerTimeLimitCheck(minutes(15), 're-run the installer');
  assert.equal(short.level, 'warn');
  assert.equal(short.summary, 'the scheduler ends a cycle after 15 min, sooner than the 30 min a cycle may take');
  assert.equal(short.fix, 're-run the installer');
  assert.equal(schedulerTimeLimitCheck(null, 'fix it'), null, 'a limit nothing can read adds no check');
});

test('Linux doctor reports the installed launcher correctly through the scheduler check', {
  skip: process.platform === 'win32' ? 'The mocked Linux doctor requires a POSIX filesystem home.' : false,
}, async () => withHome(async (home) => {
  writeConfig(home, { configVersion: 3, services: [], actions: [], updates: { automatic: false } });
  const platform = Object.getOwnPropertyDescriptor(process, 'platform');
  const originalSpawn = childProcess.spawnSync;
  let command = `"${home}/bin/legionctl" cycle`;
  childProcess.spawnSync = (file, args) => ({
    status: 0, stderr: '',
    stdout: file === 'systemctl' && args.includes('cat') ? `[Service]\nExecStart=${command}\n` : 'enabled\n',
  });
  syncBuiltinESMExports();
  Object.defineProperty(process, 'platform', { value: 'linux' });
  try {
    const loaded = loadConfig({ readOnly: true });
    const result = await runDoctor(loaded);
    assert.equal(result.checks.find((check) => check.id === 'scheduler.command').level, 'ok');
    assert.equal(result.checks.some((check) => check.id === 'scheduler.timeLimit'), false, 'a unit that names no limit adds no check');
    command = `"${home}/bin/legionctl" cycle\nTimeoutStartSec=900`;
    const short = (await runDoctor(loaded)).checks.find((check) => check.id === 'scheduler.timeLimit');
    assert.equal(short.level, 'warn');
    assert.match(short.fix, /install-linux\.sh/);
    command = `"${home}/bin/legionctl" cycle`;
    command = '"/other/installation/bin/legionctl" cycle';
    const refused = await runDoctor(loaded);
    const scheduler = refused.checks.find((check) => check.id === 'scheduler.command');
    assert.equal(scheduler.level, 'warn');
    assert.ok(scheduler.fix.includes(`${home}/bin/legionctl`));
  } finally {
    Object.defineProperty(process, 'platform', platform);
    childProcess.spawnSync = originalSpawn;
    syncBuiltinESMExports();
  }
}));
