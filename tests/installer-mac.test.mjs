import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import test from 'node:test';

const repository = path.resolve(import.meta.dirname, '..');

test('Mac installer stages safely, retains config and uses a stable launchd entry', { skip: process.platform === 'win32' }, () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'legion mac installer '));
  const home = path.join(root, "home & ' spaces");
  const base = path.join(home, '.legion-control');
  const source = path.join(root, 'source');
  fs.mkdirSync(path.join(source, 'src'), { recursive: true });
  fs.copyFileSync(path.join(repository, 'agent', 'src', 'agent-swap.mjs'), path.join(source, 'src', 'agent-swap.mjs'));
  fs.cpSync(path.join(repository, 'agent', 'install'), path.join(source, 'install'), { recursive: true });
  fs.writeFileSync(path.join(source, 'package.json'), JSON.stringify({ version: '3.0.0', type: 'module' }));
  fs.writeFileSync(path.join(source, 'src', 'index.mjs'), "process.stdout.write(JSON.stringify({ok:true,agentVersion:'3.0.0',contract:3,selfTest:{ok:true}})+'\\n');\n");
  fs.mkdirSync(base, { recursive: true });
  const config = '{"sentinel":"preserve"}\n';
  fs.writeFileSync(path.join(base, 'config.json'), config);
  const args = [path.join(repository, 'mac', 'install', 'install-mac-agent.sh'), '--skip-scheduler'];
  const env = { ...process.env, HOME: home, LEGIONCTL_HOME: base, LEGION_CONTROL_AGENT_SRC: source, LEGION_NODE_BIN: process.execPath };
  const result = spawnSync('bash', args, { encoding: 'utf8', env });
  assert.equal(result.status, 0, result.stderr);
  assert.equal(fs.readFileSync(path.join(base, 'config.json'), 'utf8'), config);
  // Only this user may enter the base, and the config in it is this user's alone.
  assert.equal(fs.statSync(base).mode & 0o777, 0o700);
  assert.equal(fs.statSync(path.join(base, 'config.json')).mode & 0o077, 0);
  assert.equal(fs.existsSync(path.join(home, 'Library', 'LaunchAgents')), false);
  const version = spawnSync(path.join(base, 'bin', 'legionctl'), ['version', '--check'], { encoding: 'utf8', env });
  assert.equal(version.status, 0, version.stderr);
  assert.equal(JSON.parse(version.stdout).selfTest.ok, true);

  const sshPath = path.join(root, 'ssh-path');
  fs.mkdirSync(sshPath);
  fs.symlinkSync('/usr/bin/dirname', path.join(sshPath, 'dirname'));
  const sshEnvironment = { HOME: home, PATH: sshPath };
  const missingNode = spawnSync('/bin/sh', ['-c', 'command -v node'], { encoding: 'utf8', env: sshEnvironment });
  assert.notEqual(missingNode.status, 0);
  const discovery = spawnSync('/bin/sh', ['-c', '~/.legion-control/bin/legionctl config'], { encoding: 'utf8', env: sshEnvironment });
  assert.equal(discovery.status, 0, discovery.stderr);
  assert.equal(JSON.parse(discovery.stdout).ok, true);

  fs.writeFileSync(path.join(source, 'src', 'index.mjs'), "process.stdout.write(JSON.stringify({ok:true,agentVersion:'3.0.0',contract:3,selfTest:{ok:false}})+'\\n');\n");
  const failed = spawnSync('bash', args, { encoding: 'utf8', env });
  assert.equal(failed.status, 1);
  const after = spawnSync(path.join(base, 'bin', 'legionctl'), ['version', '--check'], { encoding: 'utf8', env });
  assert.equal(JSON.parse(after.stdout).selfTest.ok, true);

  const plist = fs.readFileSync(path.join(repository, 'mac', 'install', 'com.x1f4r.legion-control.update.plist'), 'utf8');
  assert.match(plist, /<string>@LAUNCHER@<\/string>/);
  assert.doesNotMatch(plist, /src\/index\.mjs/);
  // What a cycle writes is this user's alone, as the Linux unit's UMask=0077 has it: 63 is 0o77.
  assert.match(plist, /<key>Umask<\/key>\s*<integer>63<\/integer>/);

  if (process.platform === 'darwin') {
    // Render and parse the real plist while every launchctl command goes only
    // to a temporary stub. No launchd job is registered on the test machine.
    fs.writeFileSync(path.join(source, 'src', 'index.mjs'), "process.stdout.write(JSON.stringify({ok:true,agentVersion:'3.0.0',contract:3,selfTest:{ok:true}})+'\\n');\n");
    const commands = path.join(root, 'commands');
    fs.mkdirSync(commands);
    fs.writeFileSync(path.join(commands, 'launchctl'), '#!/bin/sh\ncase "$1" in print) echo "state = waiting" ;; esac\nexit 0\n', { mode: 0o755 });
    const scheduler = spawnSync('bash', [args[0]], { encoding: 'utf8', env: { ...env, PATH: `${commands}:${process.env.PATH}` } });
    assert.equal(scheduler.status, 0, scheduler.stderr);
    const renderedPath = path.join(home, 'Library', 'LaunchAgents', 'com.x1f4r.legion-control.update.plist');
    const extracted = spawnSync('/usr/bin/plutil', ['-extract', 'ProgramArguments', 'json', '-o', '-', renderedPath], { encoding: 'utf8' });
    assert.equal(extracted.status, 0, extracted.stderr);
    assert.deepEqual(JSON.parse(extracted.stdout), [process.execPath, path.join(base, 'bin', 'launcher.mjs'), 'cycle']);
    const umask = spawnSync('/usr/bin/plutil', ['-extract', 'Umask', 'raw', '-o', '-', renderedPath], { encoding: 'utf8' });
    assert.equal(umask.status, 0, umask.stderr);
    assert.equal(Number(umask.stdout.trim()), 0o77);
    // The plist says where the agent and its records live, and is this user's alone as well.
    assert.equal(fs.statSync(renderedPath).mode & 0o077, 0);
  }
});

test('Mac uninstaller names what else removing the agent home would delete', { skip: process.platform === 'win32' }, (t) => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'legion mac uninstall '));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const home = path.join(root, 'home');
  const base = path.join(home, '.legion-control');
  fs.mkdirSync(path.join(base, 'agent'), { recursive: true });
  // Every launchctl command goes to a stub that has nothing loaded, so no launchd job on the test
  // machine is asked about or unloaded.
  const commands = path.join(root, 'commands');
  fs.mkdirSync(commands);
  fs.writeFileSync(path.join(commands, 'launchctl'), '#!/bin/sh\nexit 1\n', { mode: 0o755 });
  const env = { ...process.env, HOME: home, LEGIONCTL_HOME: base, PATH: `${commands}:${process.env.PATH}` };
  const uninstall = () => spawnSync('bash', [path.join(repository, 'mac', 'install', 'install-mac-agent.sh'), '--uninstall'], { encoding: 'utf8', env });

  const agentOnly = uninstall();
  assert.equal(agentOnly.status, 0, agentOnly.stderr);
  const hint = /Remove those too, if that is what you want:\n {4}(rm -rf .+)\n\n {2}Put the job back:\n/.exec(agentOnly.stdout);
  assert.ok(hint, agentOnly.stdout);
  assert.doesNotMatch(agentOnly.stdout, /not the agent's/);
  // Pasted into a shell, the hint removes the agent home and nothing else, spaces and all. The
  // shell only splits it into words here; nothing runs it.
  const words = spawnSync('bash', ['-c', `set -- ${hint[1]}; printf '%s\\0' "$@"`], { encoding: 'utf8' });
  assert.deepEqual(words.stdout.split('\0').slice(0, -1), ['rm', '-rf', base]);

  // A Mac releases are cut on: its keys and the private terms live in the same directory.
  fs.writeFileSync(path.join(base, 'release-signing.key'), 'key\n');
  fs.writeFileSync(path.join(base, 'android-release.jks'), 'keystore\n');
  fs.mkdirSync(path.join(base, 'private'));
  fs.writeFileSync(path.join(base, 'private', 'terms.txt'), 'terms\n');
  const releasing = uninstall();
  assert.equal(releasing.status, 0, releasing.stderr);
  const named = /rm -rf .+\n\n {2}That also deletes these, which are not the agent's\. Move them out first:\n((?: {4}.+\n)+)/.exec(releasing.stdout);
  assert.ok(named, releasing.stdout);
  assert.deepEqual(named[1].trimEnd().split('\n').map(line => line.trim()), [
    path.join(base, 'release-signing.key'),
    path.join(base, 'android-release.jks'),
    path.join(base, 'private'),
  ]);
  assert.equal(fs.readFileSync(path.join(base, 'private', 'terms.txt'), 'utf8'), 'terms\n');
});

// Uninstalling would otherwise suggest an rm -rf of the home directory, and installing would make it
// this user's alone.
test('Mac installer refuses a base that holds the home directory', { skip: process.platform === 'win32' }, (t) => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'legion mac home base '));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const home = path.join(root, 'home');
  fs.mkdirSync(path.join(home, 'bin'), { recursive: true });
  const commands = path.join(root, 'commands');
  fs.mkdirSync(commands);
  fs.writeFileSync(path.join(commands, 'launchctl'), '#!/bin/sh\nexit 1\n', { mode: 0o755 });
  for (const args of [['--skip-scheduler'], ['--uninstall']]) {
    const result = spawnSync('bash', [path.join(repository, 'mac', 'install', 'install-mac-agent.sh'), ...args], {
      encoding: 'utf8',
      env: { ...process.env, HOME: home, LEGIONCTL_HOME: home, LEGION_NODE_BIN: process.execPath, PATH: `${commands}:${process.env.PATH}` },
    });
    assert.equal(result.status, 1, `${args.join(' ')}\n${result.stdout}\n${result.stderr}`);
    assert.match(result.stderr, /which is not a folder of the agent's own but holds the home directory\. .* Nothing was changed\./);
    assert.doesNotMatch(result.stdout, /rm -rf/);
  }
  assert.deepEqual(fs.readdirSync(home), ['bin']);
});
