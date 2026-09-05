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
  assert.equal(fs.existsSync(path.join(home, 'Library', 'LaunchAgents')), false);
  const version = spawnSync(path.join(base, 'bin', 'legionctl'), ['version', '--check'], { encoding: 'utf8', env });
  assert.equal(version.status, 0, version.stderr);
  assert.equal(JSON.parse(version.stdout).selfTest.ok, true);

  fs.writeFileSync(path.join(source, 'src', 'index.mjs'), "process.stdout.write(JSON.stringify({ok:true,agentVersion:'3.0.0',contract:3,selfTest:{ok:false}})+'\\n');\n");
  const failed = spawnSync('bash', args, { encoding: 'utf8', env });
  assert.equal(failed.status, 1);
  const after = spawnSync(path.join(base, 'bin', 'legionctl'), ['version', '--check'], { encoding: 'utf8', env });
  assert.equal(JSON.parse(after.stdout).selfTest.ok, true);

  const plist = fs.readFileSync(path.join(repository, 'mac', 'install', 'com.x1f4r.legion-control.update.plist'), 'utf8');
  assert.match(plist, /<string>@LAUNCHER@<\/string>/);
  assert.doesNotMatch(plist, /src\/index\.mjs/);

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
  }
});
