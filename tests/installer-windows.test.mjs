import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import test from 'node:test';
import { checkoutFiles } from '../scripts/checkout-files.mjs';

const repository = path.resolve(import.meta.dirname, '..');
const installer = fs.readFileSync(path.join(repository, 'agent', 'install', 'install-windows.ps1'), 'utf8');
const service = fs.readFileSync(path.join(repository, 'agent', 'install', 'legion-control-update.service'), 'utf8');

test('Windows installer stages before moving live and uses the stable cycle launcher', () => {
  const stage = installer.indexOf("$Stage = Join-Path $Base");
  const selfCheck = installer.indexOf('Test-AgentSelfCheck $Stage');
  const park = installer.indexOf("& $NodeExe (Join-Path $PSScriptRoot 'promote.mjs') $Base $Stage");
  assert.ok(stage >= 0 && selfCheck > stage && park > selfCheck);
  assert.match(installer, /\$MinNodeMajor\s+= 24/);
  assert.match(installer, /\$LauncherArguments = .*launcher/i);
  assert.match(installer, /New-ScheduledTaskAction -Execute \$NodeExe -Argument \$LauncherArguments/);
  assert.match(installer, /inert defaults apply \(no services, no boot targets, automatic maintenance off\)/);
  assert.doesNotMatch(installer, /contract requires space free paths/i);
});

test('scheduler template invokes only the stable launcher and cycles all services', () => {
  assert.match(service, /ExecStart="@LAUNCHER@" cycle/);
  assert.doesNotMatch(service, /src\/index\.mjs/);
});

// Windows PowerShell reads a script without a byte order mark in the ANSI code page, so anything
// but ASCII in one is read as something else on a machine with another code page.
test('every PowerShell script is ASCII, which Windows PowerShell reads the same everywhere', () => {
  const scripts = checkoutFiles(repository, ['*.ps1', '*.psm1', '*.psd1']);
  assert.ok(scripts.includes('agent/install/install-windows.ps1'), scripts.join('\n'));
  for (const file of scripts) {
    const bytes = fs.readFileSync(path.join(repository, file));
    const at = bytes.findIndex(byte => byte > 0x7e || (byte < 0x20 && ![0x09, 0x0a, 0x0d].includes(byte)));
    assert.equal(at, -1, `${file} has a byte that is not ASCII at offset ${at}`);
  }
});

// --- A real install, on Windows ---------------------------------------------------

// Windows PowerShell, the way an SSH session and the documented command run the installer, with the
// scheduler left alone: the test runs as whoever the runner is and must not leave a task behind.
function runInstaller(source, base, home, args = []) {
  const script = path.join(source, 'install', 'install-windows.ps1');
  return spawnSync('powershell.exe', ['-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-File', script, '-SkipScheduler', ...args], {
    encoding: 'utf8',
    timeout: 180000,
    env: { ...process.env, PATH: `${path.dirname(process.execPath)}${path.delimiter}${process.env.PATH}`, LEGIONCTL_HOME: base, USERPROFILE: home },
  });
}

// The agent of this checkout, copied where its path needs quoting: the installer stages it, runs
// its self-check and promotes it exactly as it would a release.
function agentSource(root) {
  const agent = path.join(root, "source (x86) with ' apostrophe", 'agent');
  for (const part of ['src', 'install']) fs.cpSync(path.join(repository, 'agent', part), path.join(agent, part), { recursive: true });
  fs.copyFileSync(path.join(repository, 'agent', 'package.json'), path.join(agent, 'package.json'));
  return agent;
}

// A task name of its own, so the scheduler the installer was told to leave alone can be asked.
const taskName = () => `Legion Control test ${process.pid} ${Date.now()}`;
const taskExists = (name) => spawnSync('schtasks.exe', ['/Query', '/TN', name], { encoding: 'utf8' }).status === 0;
const versionOf = (tree) => JSON.parse(fs.readFileSync(path.join(tree, 'package.json'), 'utf8')).version;

test('the Windows installer installs and verifies a real agent, and leaves the scheduler alone when told to', { skip: process.platform !== 'win32' }, (t) => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'legion windows '));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const home = path.join(root, "home with ' apostrophe");
  const base = path.join(home, '.legion-control');
  fs.mkdirSync(base, { recursive: true });
  const config = '{"sentinel":"leave exactly alone"}\n';
  fs.writeFileSync(path.join(base, 'config.json'), config);
  const source = agentSource(root);
  const name = taskName();
  const version = versionOf(path.join(repository, 'agent'));

  const first = runInstaller(source, base, home, ['-TaskName', name]);
  assert.equal(first.status, 0, `${first.stdout}\n${first.stderr}`);
  assert.match(first.stdout, /source verification: unsigned source checkout/);
  assert.match(first.stdout, /stable launcher installed at .* and passed version --check/);
  assert.match(first.stdout, /scheduled task left untouched by -SkipScheduler/);
  assert.match(first.stdout, /config\.json is already there and was left untouched/);
  assert.equal(fs.readFileSync(path.join(base, 'config.json'), 'utf8'), config);
  assert.equal(versionOf(path.join(base, 'agent')), version);
  for (const file of ['legionctl.ps1', 'launcher.mjs', 'dispatch.ps1']) assert.equal(fs.existsSync(path.join(base, 'bin', file)), true, file);
  assert.equal(taskExists(name), false);

  // The launcher it wrote runs the agent it installed.
  const launched = spawnSync('powershell.exe', ['-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-File', path.join(base, 'bin', 'legionctl.ps1'), 'version', '--check'], { encoding: 'utf8', timeout: 60000 });
  assert.equal(launched.status, 0, launched.stderr);
  const reply = JSON.parse(launched.stdout);
  assert.equal(reply.ok, true);
  assert.equal(reply.agentVersion, version);
  assert.equal(reply.selfTest.ok, true);

  // Run again, it replaces the tree and keeps the one it replaced.
  const second = runInstaller(source, base, home, ['-TaskName', name]);
  assert.equal(second.status, 0, `${second.stdout}\n${second.stderr}`);
  assert.match(second.stdout, /previous agent retained at /);
  assert.equal(versionOf(path.join(base, 'agent.prev')), version);
  assert.equal(taskExists(name), false);
});

test('the Windows installer keeps the live agent when the staged one fails its self-check', { skip: process.platform !== 'win32' }, (t) => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'legion windows rollback '));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const home = path.join(root, 'home');
  const base = path.join(home, '.legion-control');
  const good = agentSource(root);
  const installed = runInstaller(good, base, home);
  assert.equal(installed.status, 0, `${installed.stdout}\n${installed.stderr}`);
  const version = versionOf(path.join(base, 'agent'));

  const bad = path.join(root, 'broken', 'agent');
  fs.cpSync(good, bad, { recursive: true });
  fs.writeFileSync(path.join(bad, 'package.json'), JSON.stringify({ name: 'legionctl', version: '9.9.9', type: 'module' }));
  fs.writeFileSync(path.join(bad, 'src', 'index.mjs'), "process.stdout.write(JSON.stringify({ ok: true, agentVersion: '9.9.9', contract: 3, selfTest: { ok: false } }) + '\\n');\n");
  const refused = runInstaller(bad, base, home);
  assert.equal(refused.status, 1, `${refused.stdout}\n${refused.stderr}`);
  assert.match(`${refused.stdout}\n${refused.stderr}`, /did not pass version --check/);
  assert.equal(versionOf(path.join(base, 'agent')), version);
});
