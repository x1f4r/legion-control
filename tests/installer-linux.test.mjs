import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawn, spawnSync } from 'node:child_process';
import http from 'node:http';
import test from 'node:test';
import { DatabaseSync } from 'node:sqlite';

const repository = path.resolve(import.meta.dirname, '..');
const installFiles = ['install-linux.sh', 'promote.mjs', 'launcher.mjs', 'launcher.sh', 'dispatch.sh', 'legion-control-update.service', 'legion-control-update.timer', 't3-code.service'];

function sourceFixture(root, { version = '3.0.0', healthy = true } = {}) {
  const agent = path.join(root, `source-${version}-${healthy}`);
  fs.mkdirSync(path.join(agent, 'src'), { recursive: true });
  fs.mkdirSync(path.join(agent, 'install'));
  fs.copyFileSync(path.join(repository, 'agent', 'src', 'agent-swap.mjs'), path.join(agent, 'src', 'agent-swap.mjs'));
  for (const file of installFiles) fs.copyFileSync(path.join(repository, 'agent', 'install', file), path.join(agent, 'install', file));
  fs.writeFileSync(path.join(agent, 'package.json'), JSON.stringify({ version, type: 'module' }));
  fs.writeFileSync(path.join(agent, 'src', 'index.mjs'), `
const command=process.argv[2];
if(command==='version') process.stdout.write(JSON.stringify({ok:true,agentVersion:${JSON.stringify(version)},contract:3,selfTest:{ok:${healthy}}})+'\\n');
else process.stdout.write(JSON.stringify({ok:true,command})+'\\n');
`);
  return agent;
}

function install(source, base, home, extraEnv = {}) {
  return spawnSync('bash', [path.join(source, 'install', 'install-linux.sh'), '--skip-scheduler'], {
    encoding: 'utf8',
    env: { ...process.env, HOME: home, LEGIONCTL_HOME: base, ...extraEnv },
  });
}

test('linux installer handles spaces and apostrophes, preserves config, and copies package metadata', { skip: process.platform === 'win32' }, () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'legion installer '));
  const home = path.join(root, "home with space and ' apostrophe");
  const base = path.join(home, '.legion-control');
  fs.mkdirSync(base, { recursive: true });
  const config = '{"sentinel":"leave exactly alone"}\n';
  fs.writeFileSync(path.join(base, 'config.json'), config);
  const source = sourceFixture(root);
  const nodeDirectory = path.join(root, "node path ' & | shell");
  fs.mkdirSync(nodeDirectory);
  const node = path.join(nodeDirectory, 'node');
  fs.symlinkSync(process.execPath, node);

  const result = install(source, base, home, { LEGION_NODE_BIN: node });
  assert.equal(result.status, 0, result.stderr);
  assert.equal(fs.readFileSync(path.join(base, 'config.json'), 'utf8'), config);
  assert.equal(JSON.parse(fs.readFileSync(path.join(base, 'agent', 'package.json'), 'utf8')).version, '3.0.0');
  assert.equal(fs.existsSync(path.join(base, 'agent', 'install', 'launcher.mjs')), true);
  const launched = spawnSync(path.join(base, 'bin', 'legionctl'), ['status'], { encoding: 'utf8' });
  assert.equal(launched.status, 0, launched.stderr);
  assert.equal(JSON.parse(launched.stdout).command, 'status');
});

test('first setup can fetch through the installed wrapper without Node on the SSH PATH', { skip: process.platform === 'win32' }, (t) => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'legion setup discovery '));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const home = path.join(root, "home with ' spaces");
  const base = path.join(home, '.legion-control');
  const source = sourceFixture(root);
  const commands = path.join(root, 'ssh-path');
  fs.mkdirSync(commands);
  fs.symlinkSync('/usr/bin/dirname', path.join(commands, 'dirname'));
  const result = install(source, base, home, { LEGION_NODE_BIN: process.execPath });
  assert.equal(result.status, 0, result.stderr);

  // A clean noninteractive connection has neither a login profile nor the
  // installer's Node directory. The wrapper must use its recorded runtime.
  const env = { HOME: home, PATH: commands };
  const missing = spawnSync('/bin/sh', ['-c', 'command -v node'], { encoding: 'utf8', env });
  assert.notEqual(missing.status, 0);
  const fetched = spawnSync('/bin/sh', ['-c', '~/.legion-control/bin/legionctl config'], { encoding: 'utf8', env });
  assert.equal(fetched.status, 0, fetched.stderr);
  assert.equal(JSON.parse(fetched.stdout).command, 'config');
});

test('linux installer rejects a failed staged self-check without replacing the live tree', { skip: process.platform === 'win32' }, () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'legion installer rollback '));
  const home = path.join(root, 'home');
  const base = path.join(home, '.legion-control');
  const good = sourceFixture(root, { version: '3.0.0', healthy: true });
  const first = install(good, base, home);
  assert.equal(first.status, 0, first.stderr);

  const bad = sourceFixture(root, { version: '3.0.1', healthy: false });
  const second = install(bad, base, home);
  assert.equal(second.status, 1);
  assert.match(second.stderr, /failed version --check/);
  assert.equal(JSON.parse(fs.readFileSync(path.join(base, 'agent', 'package.json'), 'utf8')).version, '3.0.0');
});

test('installer promotion refuses a held operation mutex and preserves the active version', { skip: process.platform === 'win32' }, () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'legion installer exclusion '));
  const home = path.join(root, 'home');
  const base = path.join(home, '.legion-control');
  const oldSource = sourceFixture(root, { version: '3.0.0' });
  const installed = install(oldSource, base, home);
  assert.equal(installed.status, 0, installed.stderr);
  const nextSource = sourceFixture(root, { version: '3.0.1' });
  const db = new DatabaseSync(path.join(base, 'op.lock.sqlite'));
  db.exec('BEGIN IMMEDIATE');
  try {
    const blocked = install(nextSource, base, home);
    assert.equal(blocked.status, 1);
    assert.match(blocked.stderr, /another operation is running/);
    assert.equal(JSON.parse(fs.readFileSync(path.join(base, 'agent', 'package.json'))).version, '3.0.0');
    assert.equal(fs.readdirSync(base).some((name) => name.startsWith('agent.install.new.')), true);
  } finally { db.exec('ROLLBACK'); db.close(); }
  const next = install(nextSource, base, home);
  assert.equal(next.status, 0, next.stderr);
  assert.equal(JSON.parse(fs.readFileSync(path.join(base, 'agent', 'package.json'))).version, '3.0.1');
  assert.equal(JSON.parse(fs.readFileSync(path.join(base, 'agent.prev', 'package.json'))).version, '3.0.0');
});

// --- The systemd units ----------------------------------------------------------

/**
 * The words systemd reads out of a setting's value (systemd.syntax(7)): % starts a specifier and
 * %% is a percent sign, quotes group, a backslash escapes, and on an Exec line $ starts a variable
 * and $$ is a dollar sign. Anything the installer never means to write, a specifier, a variable or
 * an escape other than \\ and \", fails the test rather than being read one way or another.
 */
function systemdWords(value, { exec = false } = {}) {
  assert.doesNotMatch(value.replaceAll('%%', ''), /%/, `a specifier in ${value}`);
  const text = value.replaceAll('%%', '%');
  const words = [];
  let word = null;
  let quote = null;
  for (let at = 0; at < text.length; at += 1) {
    const character = text[at];
    if (character === '\\' && quote !== "'") {
      assert.match(text[at + 1] ?? '', /^[\\"]$/, `an escape systemd reads differently in ${value}`);
      word = (word ?? '') + text[at + 1];
      at += 1;
    } else if (quote !== null) {
      if (character === quote) quote = null;
      else word += character;
    } else if (character === '"' || character === "'") {
      quote = character;
      word ??= '';
    } else if (/\s/.test(character)) {
      if (word !== null) words.push(word);
      word = null;
    } else {
      word = (word ?? '') + character;
    }
  }
  assert.equal(quote, null, `an unterminated quote in ${value}`);
  if (word !== null) words.push(word);
  if (!exec) return words;
  return words.map((each) => {
    assert.doesNotMatch(each.replaceAll('$$', ''), /\$/, `a variable in ${value}`);
    return each.replaceAll('$$', '$');
  });
}

/** Each setting of a unit file in order, as [key, value], comments left out. */
const settings = (text) => text.split('\n').filter((line) => /^[A-Za-z]+=/.test(line)).map((line) => [line.slice(0, line.indexOf('=')), line.slice(line.indexOf('=') + 1)]);
const values = (unit, key, options) => settings(unit).filter(([name]) => name === key).map(([, value]) => systemdWords(value, options));

// A systemctl that only notes what it is asked. The units are rendered and written for real, on a
// machine that may have no user manager, or one this test must not touch. LEGION_TEST_ACTIVE is
// what it says a unit is doing.
function fakeCommands(root) {
  const bin = path.join(root, 'fake-bin');
  fs.mkdirSync(bin, { recursive: true });
  fs.writeFileSync(path.join(bin, 'systemctl'), `#!/bin/sh
printf '%s\\n' "$*" >> "$LEGION_TEST_SYSTEMCTL"
case "$*" in
  *' cat '*) exit 1 ;;
  *is-enabled*) echo enabled ;;
  *is-active*) echo "\${LEGION_TEST_ACTIVE:-inactive}" ;;
esac
exit 0
`, { mode: 0o755 });
  return bin;
}

// Every character a unit or a sed replacement reads as syntax, in the home directory the units are
// written for, which holds the npm prefix and the launcher. All but the backslash, which the
// launcher's wrapper cannot run from; the npm prefix of the second test has one.
const AWKWARD_HOME = `home with space, $HOME \${HOME}, 100% & | "quotes" and ' apostrophe`;

function unitInstall(root, home, env = {}) {
  const calls = path.join(root, 'systemctl-calls');
  const options = {
    encoding: 'utf8',
    env: { ...process.env, PATH: `${fakeCommands(root)}${path.delimiter}${process.env.PATH}`, HOME: home, LEGIONCTL_HOME: path.join(home, '.legion-control'), LEGION_NODE_BIN: process.execPath, LEGION_TEST_SYSTEMCTL: calls, ...env },
  };
  return { options, calls: () => fs.readFileSync(calls, 'utf8').trim().split('\n') };
}

const unitSkip = process.platform === 'win32' || process.getuid?.() === 0;

test('linux installer writes units that systemd reads back as the paths it meant, for a home with a space in it', { skip: unitSkip }, (t) => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'legion units '));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const home = path.join(root, AWKWARD_HOME);
  const units = path.join(home, '.config', 'systemd', 'user');
  fs.mkdirSync(units, { recursive: true });
  // A unit left from when the update pair was named after the one service it drove.
  fs.writeFileSync(path.join(units, 'legion-t3-update.timer'), '[Timer]\n');
  const source = sourceFixture(root);
  const { options, calls } = unitInstall(root, home);

  const result = spawnSync('bash', [path.join(source, 'install', 'install-linux.sh')], options);
  assert.equal(result.status, 0, result.stderr);

  const service = fs.readFileSync(path.join(units, 'legion-control-update.service'), 'utf8');
  const uid = process.getuid();
  assert.deepEqual(values(service, 'Environment').flat(), [
    `PATH=${path.join(home, '.npm-global', 'bin')}:/usr/local/bin:/usr/bin:/bin`,
    `XDG_RUNTIME_DIR=/run/user/${uid}`,
    `DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/${uid}/bus`,
  ]);
  const execStart = values(service, 'ExecStart', { exec: true });
  assert.deepEqual(execStart, [[path.join(home, '.legion-control', 'bin', 'legionctl'), 'cycle']]);
  // What the cycle writes, and what it runs writes, is this user's alone.
  assert.deepEqual(values(service, 'UMask'), [['0077']]);
  // And that is the launcher, which runs.
  const launched = spawnSync(execStart[0][0], ['status'], { encoding: 'utf8' });
  assert.equal(launched.status, 0, launched.stderr);
  assert.equal(fs.readFileSync(path.join(units, 'legion-control-update.timer'), 'utf8'), fs.readFileSync(path.join(repository, 'agent', 'install', 'legion-control-update.timer'), 'utf8'));
  assert.equal(fs.existsSync(path.join(units, 't3-code.service')), false);

  // The timer pulls the oneshot service in, so the timer is the only unit enabled, and the unit of
  // the old name is retired.
  const asked = calls();
  assert.ok(asked.includes('--user show-environment'), asked.join('\n'));
  assert.ok(asked.includes('--user daemon-reload'), asked.join('\n'));
  assert.ok(asked.includes('--user disable --now legion-t3-update.timer'), asked.join('\n'));
  assert.deepEqual(asked.filter((call) => call.startsWith('--user enable')), ['--user enable --now legion-control-update.timer']);
  assert.equal(fs.existsSync(path.join(units, 'legion-t3-update.timer')), false);

  // Rendering again changes nothing.
  const again = spawnSync('bash', [path.join(source, 'install', 'install-linux.sh')], options);
  assert.equal(again.status, 0, again.stderr);
  assert.match(again.stderr, /legion-control-update\.service unchanged/);
});

test('linux installer quotes and escapes the npm prefix in both units it writes for the reference service', { skip: unitSkip }, async (t) => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'legion units t3 '));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const home = path.join(root, AWKWARD_HOME);
  const prefix = path.join(root, 'npm prefix with a back\\slash, & | % $PATH "quotes"');
  // T3 already installed, so the installer leaves npm alone; this npm is only there to be found.
  const npmBin = path.join(prefix, 'bin');
  fs.mkdirSync(path.join(prefix, 'lib', 'node_modules', 't3'), { recursive: true });
  fs.writeFileSync(path.join(prefix, 'lib', 'node_modules', 't3', 'package.json'), '{"version":"0.0.1"}\n');
  fs.mkdirSync(npmBin, { recursive: true });
  fs.writeFileSync(path.join(npmBin, 't3'), '#!/bin/sh\n', { mode: 0o755 });
  fs.writeFileSync(path.join(fakeCommands(root), 'npm'), '#!/bin/sh\necho 11.0.0\n', { mode: 0o755 });
  // The installer waits for the service to answer on its port before it says it is done.
  const server = http.createServer((request, response) => response.end('ok'));
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  t.after(() => server.close());
  const port = server.address().port;
  const source = sourceFixture(root);
  const { options } = unitInstall(root, home, { LEGION_T3_PORT: String(port), LEGION_NPM_PREFIX: prefix });

  // Asynchronously, so the server above can answer the installer's health check.
  const result = await new Promise((resolve) => {
    const child = spawn('bash', [path.join(source, 'install', 'install-linux.sh'), '--with-t3'], options);
    let stderr = '';
    child.stderr.on('data', (chunk) => { stderr += chunk; });
    child.stdout.resume();
    child.on('close', (status) => resolve({ status, stderr }));
  });
  assert.equal(result.status, 0, result.stderr);
  assert.match(result.stderr, new RegExp(`http://127\\.0\\.0\\.1:${port}/ answered 200`));

  const unitPath = `PATH=${npmBin}:/usr/local/bin:/usr/bin:/bin`;
  const update = fs.readFileSync(path.join(home, '.config', 'systemd', 'user', 'legion-control-update.service'), 'utf8');
  assert.equal(values(update, 'Environment')[0][0], unitPath);
  const unit = fs.readFileSync(path.join(home, '.config', 'systemd', 'user', 't3-code.service'), 'utf8');
  assert.deepEqual(values(unit, 'Environment'), [[unitPath]]);
  assert.deepEqual(values(unit, 'ExecStart', { exec: true }), [
    [path.join(npmBin, 't3'), 'start', '--mode', 'web', '--host', '127.0.0.1', '--port', String(port), '--no-browser'],
  ]);
});

test('linux installer refuses a reference service port that is not a number', { skip: process.platform === 'win32' }, (t) => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'legion units port '));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const home = path.join(root, 'home');
  // An npm that fails, so an installer that let the port through stops here and goes nowhere near
  // the network.
  const bin = fakeCommands(root);
  fs.writeFileSync(path.join(bin, 'npm'), '#!/bin/sh\nexit 1\n', { mode: 0o755 });
  const result = spawnSync('bash', [path.join(sourceFixture(root), 'install', 'install-linux.sh'), '--with-t3', '--skip-scheduler'], {
    encoding: 'utf8',
    env: { ...process.env, PATH: `${bin}${path.delimiter}${process.env.PATH}`, HOME: home, LEGIONCTL_HOME: path.join(home, '.legion-control'), LEGION_T3_PORT: '3773 --host 0.0.0.0' },
  });
  assert.equal(result.status, 1);
  assert.match(result.stderr, /LEGION_T3_PORT must be a port number, got '3773 --host 0\.0\.0\.0'\./);
  assert.equal(fs.existsSync(path.join(home, '.legion-control')), false);
});

// A user manager has units of its own and none of the system manager's, so ordering on
// network-online.target, or on any other unit only the system manager has, does nothing at all.
test('the user units order on no unit that only the system manager has', () => {
  for (const name of ['legion-control-update.service', 'legion-control-update.timer', 't3-code.service']) {
    const unit = fs.readFileSync(path.join(repository, 'agent', 'install', name), 'utf8');
    const named = settings(unit)
      .filter(([key]) => /^(?:After|Before|Wants|Requires|Requisite|BindsTo|PartOf|Upholds)$/.test(key))
      .flatMap(([, value]) => value.trim().split(/\s+/));
    assert.deepEqual(named.filter((each) => /^(?:network|network-online|network-pre|nss-lookup|time-sync|remote-fs)\.target$/.test(each)), [], name);
  }
});

// --- File modes ------------------------------------------------------------------

/** Every entry under `directory` that its group or anybody else may read, write or enter, with its mode. */
function openToOthers(directory) {
  const open = [];
  const walk = (entry) => {
    const stat = fs.lstatSync(entry);
    if ((stat.mode & 0o077) !== 0 && !stat.isSymbolicLink()) open.push(`${path.relative(directory, entry) || '.'} ${(stat.mode & 0o777).toString(8)}`);
    if (stat.isDirectory()) for (const name of fs.readdirSync(entry)) walk(path.join(entry, name));
  };
  walk(directory);
  return open;
}

test('linux installer leaves nothing in the base that another user can read, also in a base an older installer left open', { skip: process.platform === 'win32' }, (t) => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'legion installer modes '));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const home = path.join(root, 'home with space');
  const base = path.join(home, '.legion-control');
  // What an older installer and agent left behind under a umask of 022.
  fs.mkdirSync(path.join(base, 'state'), { recursive: true });
  fs.writeFileSync(path.join(base, 'config.json'), '{"sentinel":"leave exactly alone"}\n');
  fs.writeFileSync(path.join(base, 'state', 'tower.json'), '{}\n');
  for (const [entry, mode] of [['', 0o755], ['state', 0o755], ['config.json', 0o644], [path.join('state', 'tower.json'), 0o644]]) fs.chmodSync(path.join(base, entry), mode);

  const result = install(sourceFixture(root), base, home, { LEGION_NODE_BIN: process.execPath });
  assert.equal(result.status, 0, result.stderr);
  assert.equal(fs.statSync(base).mode & 0o777, 0o700);
  assert.deepEqual(openToOthers(base), []);
  assert.equal(fs.readFileSync(path.join(base, 'config.json'), 'utf8'), '{"sentinel":"leave exactly alone"}\n');
  // Private, and still the launcher its user runs.
  const launched = spawnSync(path.join(base, 'bin', 'legionctl'), ['status'], { encoding: 'utf8' });
  assert.equal(launched.status, 0, launched.stderr);
});

// --- Uninstall ---------------------------------------------------------------------

test('linux uninstall removes the timer, its units, the launchers and the agent, and keeps its config and records until --purge', { skip: unitSkip }, (t) => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'legion uninstall '));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const home = path.join(root, AWKWARD_HOME);
  const base = path.join(home, '.legion-control');
  const units = path.join(home, '.config', 'systemd', 'user');
  const script = path.join(sourceFixture(root), 'install', 'install-linux.sh');
  const { options, calls } = unitInstall(root, home);
  // Installed twice, so the tree the second install replaced is retained beside the live one.
  for (let run = 0; run < 2; run += 1) {
    const installed = spawnSync('bash', [script], options);
    assert.equal(installed.status, 0, installed.stderr);
  }
  assert.equal(fs.existsSync(path.join(base, 'agent.prev')), true);
  // What the agent was configured with and what it recorded, and something that is not the agent's.
  const records = {
    'config.json': '{"sentinel":"leave exactly alone"}\n',
    'config.last-good.json': '{}\n',
    'controller.json': '{}\n',
    'agent-floor.json': '{}\n',
    'legionctl.log': 'one line\n',
    [path.join('state', 'tower.json')]: '{}\n',
    [path.join('ops', 'op-1.json')]: '{}\n',
  };
  for (const [file, text] of Object.entries(records)) {
    fs.mkdirSync(path.dirname(path.join(base, file)), { recursive: true });
    fs.writeFileSync(path.join(base, file), text);
  }
  fs.mkdirSync(path.join(base, 'private'));
  fs.writeFileSync(path.join(base, 'private', 'terms.txt'), 'terms\n');
  fs.mkdirSync(path.join(home, '.ssh'));
  fs.writeFileSync(path.join(home, '.ssh', 'authorized_keys'), `command="${path.join(base, 'bin', 'dispatch.sh')}",restrict ssh-ed25519 AAAA phone\n`);

  const removed = spawnSync('bash', [script, '--uninstall'], options);
  assert.equal(removed.status, 0, removed.stderr);
  for (const unit of ['legion-control-update.timer', 'legion-control-update.service']) assert.equal(fs.existsSync(path.join(units, unit)), false, unit);
  const asked = calls();
  assert.ok(asked.includes('--user disable --now legion-control-update.timer'), asked.join('\n'));
  assert.equal(asked.at(-1), '--user daemon-reload');
  for (const gone of ['bin', 'agent', 'agent.prev']) assert.equal(fs.existsSync(path.join(base, gone)), false, gone);
  for (const [file, text] of Object.entries(records)) assert.equal(fs.readFileSync(path.join(base, file), 'utf8'), text, file);
  assert.equal(fs.readFileSync(path.join(base, 'private', 'terms.txt'), 'utf8'), 'terms\n');
  assert.match(removed.stdout, /were left in:\n {4}.+\n\n {2}Remove those too:\n {4}bash .+ --uninstall --purge\n/);
  assert.match(removed.stdout, /runs dispatch\.sh, which is gone, so it is refused from now on/);

  // Purged, everything of the agent's goes, and what is not the agent's stays and is named.
  const purged = spawnSync('bash', [script, '--uninstall', '--purge'], options);
  assert.equal(purged.status, 0, purged.stderr);
  assert.deepEqual(fs.readdirSync(base), ['private']);
  assert.ok(purged.stdout.includes(`kept ${path.join(base, 'private')}, which is not the agent's`), purged.stdout);
  assert.equal(fs.readFileSync(path.join(base, 'private', 'terms.txt'), 'utf8'), 'terms\n');
  fs.rmSync(path.join(base, 'private'), { recursive: true });
  const emptied = spawnSync('bash', [script, '--uninstall', '--purge'], options);
  assert.equal(emptied.status, 0, emptied.stderr);
  assert.equal(fs.existsSync(base), false);
});

test('linux uninstall changes nothing while a cycle runs or an operation holds the lock', { skip: unitSkip }, (t) => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'legion uninstall busy '));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const home = path.join(root, 'home');
  const base = path.join(home, '.legion-control');
  const units = path.join(home, '.config', 'systemd', 'user');
  const script = path.join(sourceFixture(root), 'install', 'install-linux.sh');
  const { options } = unitInstall(root, home);
  const installed = spawnSync('bash', [script], options);
  assert.equal(installed.status, 0, installed.stderr);
  const untouched = () => {
    for (const entry of [path.join(base, 'bin', 'legionctl'), path.join(base, 'agent', 'src', 'index.mjs'), path.join(units, 'legion-control-update.timer'), path.join(units, 'legion-control-update.service')]) {
      assert.equal(fs.existsSync(entry), true, entry);
    }
  };

  const running = spawnSync('bash', [script, '--uninstall'], { ...options, env: { ...options.env, LEGION_TEST_ACTIVE: 'activating' } });
  assert.equal(running.status, 1);
  assert.match(running.stderr, /an update cycle is running right now\. .* Nothing was changed\./);
  untouched();

  const db = new DatabaseSync(path.join(base, 'op.lock.sqlite'));
  db.exec('BEGIN IMMEDIATE');
  try {
    const locked = spawnSync('bash', [script, '--uninstall', '--purge'], options);
    assert.equal(locked.status, 1);
    assert.match(locked.stderr, /another operation is running; nothing was removed/);
    untouched();
  } finally { db.exec('ROLLBACK'); db.close(); }
});

// Made this user's alone, the home directory would shut out everybody else, and uninstalling from
// it would take its bin folder. So would a folder that holds it.
test('linux installer refuses a base that holds the home directory, installing or uninstalling', { skip: unitSkip }, (t) => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'legion home base '));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const home = path.join(root, 'home');
  fs.mkdirSync(path.join(home, 'bin'), { recursive: true });
  fs.writeFileSync(path.join(home, 'bin', 'tool'), 'mine\n');
  fs.chmodSync(home, 0o755);
  const script = path.join(sourceFixture(root), 'install', 'install-linux.sh');
  const { options } = unitInstall(root, home);
  for (const base of [home, `${home}/`, root]) {
    for (const args of [['--skip-scheduler'], [], ['--uninstall'], ['--uninstall', '--purge']]) {
      const result = spawnSync('bash', [script, ...args], { ...options, env: { ...options.env, LEGIONCTL_HOME: base } });
      assert.equal(result.status, 1, `${base} ${args.join(' ')}\n${result.stderr}`);
      assert.match(result.stderr, /LEGIONCTL_HOME is .*, which is not a folder of the agent's own but holds the home directory\. .* Nothing was changed\./);
    }
  }
  assert.equal(fs.statSync(home).mode & 0o777, 0o755);
  assert.deepEqual(fs.readdirSync(home), ['bin']);
  assert.equal(fs.readFileSync(path.join(home, 'bin', 'tool'), 'utf8'), 'mine\n');
  assert.equal(fs.existsSync(path.join(root, 'systemctl-calls')), false);
});

test('linux installer takes --purge only with --uninstall, and --uninstall only with --purge', { skip: process.platform === 'win32' }, (t) => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'legion uninstall arguments '));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const script = path.join(sourceFixture(root), 'install', 'install-linux.sh');
  const env = { ...process.env, HOME: path.join(root, 'home'), LEGIONCTL_HOME: path.join(root, 'home', '.legion-control') };
  for (const [args, message] of [
    [['--purge'], /--purge only goes with --uninstall\./],
    [['--uninstall', '--with-t3'], /--uninstall takes --purge and nothing else\./],
    [['--uninstall', '--skip-scheduler'], /--uninstall takes --purge and nothing else\./],
  ]) {
    const result = spawnSync('bash', [script, ...args], { encoding: 'utf8', env });
    assert.equal(result.status, 1, args.join(' '));
    assert.match(result.stderr, message);
  }
  assert.equal(fs.existsSync(path.join(root, 'home')), false);
});
