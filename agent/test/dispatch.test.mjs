// The restricted ssh dispatcher: the grammar, and what a restricted key may do.

import assert from 'node:assert/strict';
import path from 'node:path';
import fs from 'node:fs';
import os from 'node:os';
import { spawnSync } from 'node:child_process';
import test from 'node:test';
import { checkReply } from '../../contract/validate.mjs';
import { canonicalHash } from '../src/controller.mjs';
import { authorize, parseCommandLine, ownEntry, RESTRICTED_COMMANDS } from '../src/dispatch.mjs';
import { REPO, cli, withHome } from './helpers.mjs';

const quote = (value) => "'" + value.replace(/'/g, "'\\''") + "'";
const NODE = quote(process.execPath);
const ENTRY = quote(ownEntry());

test('bare tokens, quoted tokens and the escaped-quote idiom all parse', () => {
  assert.deepEqual(parseCommandLine('node /a/b status').tokens, ['node', '/a/b', 'status']);
  assert.deepEqual(parseCommandLine("node /a/b run 'a name with spaces'").tokens, ['node', '/a/b', 'run', 'a name with spaces']);
  // A double quote inside single quotes is literal: a Windows path can hold one.
  assert.deepEqual(parseCommandLine(`a 'he said "hi"'`).tokens, ['a', 'he said "hi"']);
  // The POSIX idiom for a literal quote: close, escape, reopen.
  assert.deepEqual(parseCommandLine("a 'it'\\''s'").tokens, ['a', "it's"]);
  assert.deepEqual(parseCommandLine('a\tb  c').tokens, ['a', 'b', 'c']);
});

test('every shell metacharacter outside quotes is refused', () => {
  for (const line of [
    'node /a/b status; rm -rf /',
    'node /a/b run $(whoami)',
    'node /a/b run `whoami`',
    'node /a/b status && curl evil',
    'node /a/b status | tee /tmp/x',
    'node /a/b status > /tmp/x',
    'node /a/b run *',
    'node /a/b run "double"',
    'node /a/b status\nrm -rf /',
  ]) {
    const parsed = parseCommandLine(line);
    assert.equal(parsed.ok, false, `${JSON.stringify(line)} should have been refused`);
    assert.match(parsed.error, /not allowed outside a quoted argument/);
  }
});

test('an unterminated quote is refused rather than guessed at', () => {
  assert.match(parseCommandLine("node /a/b run 'never closed").error, /never closed/);
});

test('the command line and its arguments are bounded', () => {
  assert.match(parseCommandLine(`node /a/b run ${'x'.repeat(5000)}`).error, /longer than/);
  assert.match(parseCommandLine('a '.repeat(100)).error, /more than 64 arguments/);
  assert.match(parseCommandLine(`node ${'y'.repeat(20000)}`).error, /longer than/);
});

test('an allowed command is authorized and handed back as argv', () => {
  const allowed = authorize(`${NODE} ${ENTRY} status`);
  assert.equal(allowed.ok, true);
  assert.deepEqual(allowed.argv, ['status']);

  const withFlags = authorize(`${NODE} ${ENTRY} update --service demo --force`);
  assert.equal(withFlags.ok, true);
  assert.deepEqual(withFlags.argv, ['update', '--service', 'demo', '--force']);
});

test('anything that is not an allowed command is refused, and the reply lists what is', () => {
  const refused = authorize('ls -la /');
  assert.equal(refused.ok, false);
  // "ls" never reaches the command check: "/" is not a usable argument. Either
  // way nothing runs, and the reply names what went wrong.
  assert.ok(['restricted', 'bad-argument'].includes(refused.reasonCode));

  const notACommand = authorize(`${NODE} ${ENTRY} rmrf`);
  assert.equal(notACommand.ok, false);
  assert.equal(notACommand.reasonCode, 'restricted');
  assert.ok(notACommand.accepts.includes('status'));
});

test('a key may only run THIS agent, not a tree it just uploaded', () => {
  const elsewhere = authorize(`${NODE} /tmp/uploaded/index.mjs status`);
  assert.equal(elsewhere.ok, false);
  assert.equal(elsewhere.reasonCode, 'restricted');
  assert.match(elsewhere.message, /may only run/);
});

test('installed stable launcher routes the same restricted commands and refuses another base', () => {
  const base = fs.mkdtempSync(path.join(os.tmpdir(), "dispatch install's "));
  try {
    fs.mkdirSync(path.join(base, 'agent', 'src'), { recursive: true }); fs.mkdirSync(path.join(base, 'bin'));
    const entry = path.join(base, 'agent', 'src', 'index.mjs'); const launcher = path.join(base, 'bin', 'launcher.mjs');
    fs.writeFileSync(entry, 'must never execute'); fs.writeFileSync(launcher, 'must never execute');
    const options = { entry: fs.realpathSync(entry) };
    for (const command of ['status', 'update --service demo', 'self-update --stdin', 'self-update --check']) {
      assert.deepEqual(authorize(`${NODE} ${quote(launcher)} ${command}`, options), authorize(`${NODE} ${quote(entry)} ${command}`, options));
    }
    for (const command of ['service-config get', 'dispatch', 'self-update --install']) assert.equal(authorize(`${NODE} ${quote(launcher)} ${command}`, options).ok, false);
    assert.equal(authorize(`${quote('/other/node')} ${quote(launcher)} status`, options).ok, false);
    const other = path.join(base, 'other', 'bin', 'launcher.mjs'); fs.mkdirSync(path.dirname(other), { recursive: true }); fs.writeFileSync(other, 'must never execute');
    assert.equal(authorize(`${NODE} ${quote(other)} status`, options).ok, false);
    fs.unlinkSync(launcher); fs.symlinkSync(other, launcher);
    assert.equal(authorize(`${NODE} ${quote(launcher)} status`, options).ok, false);
  } finally { fs.rmSync(base, { recursive: true, force: true }); }
});

test('an interpreter that is not this one is refused', () => {
  const wrong = authorize(`/usr/bin/python3 ${ENTRY} status`);
  assert.equal(wrong.ok, false);
  assert.match(wrong.message, /is not the interpreter/);
});

test('a symlinked interpreter path still resolves to this one', () => {
  // A configured path very often differs from process.execPath by a symlink; a
  // dispatcher that refused those would be unusable on the machines people have.
  assert.equal(authorize(`node ${ENTRY} status`).ok, true);
  assert.equal(authorize(`${quote(path.basename(process.execPath))} ${ENTRY} status`).ok, true);
});

test('dispatch cannot dispatch itself', () => {
  const refused = authorize(`${NODE} ${ENTRY} dispatch`);
  assert.equal(refused.ok, false);
});

test('a restricted key may upload a bundle but may not run one from a path', () => {
  assert.equal(authorize(`${NODE} ${ENTRY} self-update --stdin`).ok, true);
  assert.equal(authorize(`${NODE} ${ENTRY} self-update --check`).ok, true);
  assert.equal(authorize(`${NODE} ${ENTRY} self-update --rollback`).ok, true);

  const fromPath = authorize(`${NODE} ${ENTRY} self-update --from /tmp/agent.tgz`);
  assert.equal(fromPath.ok, false, 'a path this session chose must not be executed');

  const install = authorize(`${NODE} ${ENTRY} self-update --install`);
  assert.equal(install.ok, false);
  assert.match(install.message, /tree this session provided/);
});

test('the shape of a restricted command is judged on the argv the agent will parse', () => {
  // Each of these is --install, --from or --allow-downgrade to the CLI's parser,
  // whatever its spelling, so each is refused before the agent runs it.
  for (const shape of [
    'self-update --check --stdin --install=1',
    'self-update --stdin --install=',
    'self-update --check --from=bundle.tgz',
    'self-update --rollback --allow-downgrade=yes',
    'self-update --stdin --install',
  ]) {
    const refused = authorize(`${NODE} ${ENTRY} ${shape}`);
    assert.equal(refused.ok, false, shape);
    assert.equal(refused.reasonCode, 'restricted', shape);
  }
  // A value an option consumes is not a flag: here --op takes "--install" as its
  // id, which the CLI then refuses as a bad id rather than as an install.
  assert.equal(authorize(`${NODE} ${ENTRY} self-update --stdin --op --install`).ok, true);
  assert.equal(authorize(`${NODE} ${ENTRY} self-update --op 6f1c2a84-3b7d-4e19-9a52-0c8f4d61b7a3 --stdin`).ok, true);
  // Asking for none of the three shapes a restricted key has is refused too.
  assert.equal(authorize(`${NODE} ${ENTRY} self-update --op 6f1c2a84-3b7d-4e19-9a52-0c8f4d61b7a3`).reasonCode, 'restricted');
});

test('a restricted --install=1 never reaches the agent', async () => {
  await withHome(async (home) => {
    const run = cli(['dispatch'], {
      home,
      stdin: 'not a bundle',
      env: { LEGIONCTL_RESTRICTED: '1', SSH_ORIGINAL_COMMAND: `node ${ENTRY} self-update --check --stdin --install=1` },
    });
    assert.equal(run.payload.reasonCode, 'restricted');
    assert.match(run.payload.message, /tree this session provided/);
    assert.equal(run.payload.outcome, undefined, 'a check changes nothing, refused or not');
  });
});

test('a restricted key may not ask to go below the installed agent or the version floor', () => {
  for (const flag of ['--allow-downgrade', '--allow-downgrade=1']) {
    for (const mode of ['--stdin', '--rollback', '--check']) {
      const refused = authorize(`${NODE} ${ENTRY} self-update ${mode} ${flag}`);
      assert.equal(refused.ok, false, `${mode} ${flag}`);
      assert.equal(refused.reasonCode, 'restricted');
      assert.match(refused.message, /only an unrestricted session/);
    }
  }
});

test('the allow list is the one published to the clients', () => {
  for (const command of ['status', 'busy', 'update', 'restart', 'boot', 'sleep', 'run', 'cycle', 'op', 'cancel', 'history', 'logs', 'doctor', 'bundle', 'config', 'policy', 'version']) {
    assert.ok(RESTRICTED_COMMANDS.has(command), `${command} should be allowed for a restricted key`);
  }
  assert.equal(RESTRICTED_COMMANDS.has('dispatch'), false);
  assert.equal(RESTRICTED_COMMANDS.has('op-run'), false);
});

test('a restricted key that pushes a new launcher through the dispatcher is refused, in the contract\'s shape', async () => {
  await withHome(async (home) => {
    const document = (revision, lineage, agent) => `${JSON.stringify({
      version: 1,
      controller: { id: 'setup-dispatch', revision, lineage, source: 'cli' },
      machines: [{ id: 'tower', endpoints: [{ host: 'desk-tower.local', user: 'me' }], systems: [{ id: 'linux', agent }] }],
    }, null, 2)}\n`;
    const d0 = document(1, [], ['/usr/bin/node', '/home/me/.legion-control/bin/launcher.mjs']);
    assert.equal(cli(['config', 'set'], { home, stdin: d0 }).payload.action, 'stored');
    const d1 = document(2, [canonicalHash(d0)], ['sh', '-c', 'id']);
    const run = cli(['dispatch'], {
      home,
      stdin: d1,
      env: { LEGIONCTL_RESTRICTED: '1', SSH_ORIGINAL_COMMAND: `node ${ENTRY} config set --controller-id setup-dispatch --revision 2` },
    });
    assert.equal(run.exitCode, 1);
    assert.equal(run.payload.reasonCode, 'restricted');
    assert.deepEqual(run.payload.launchChanges, ['machines[tower].systems[linux]']);
    assert.equal(run.payload.current.hash, canonicalHash(d0));
    assert.deepEqual(checkReply('config-set.schema.json', run.payload, { where: 'restricted config set' }), []);
    assert.equal(fs.readFileSync(path.join(home, 'controller.json'), 'utf8'), d0);
  });
});

// ---------------------------------------------------------------------------
// The environment the agent starts with (T10)
// ---------------------------------------------------------------------------

const INSTALL = path.join(REPO, 'agent', 'install');

/** A script that writes what it was started with (its argv and environment) as JSON to `out`. */
function recorder(dir, out) {
  const file = path.join(dir, 'record.mjs');
  fs.writeFileSync(file, `import fs from 'node:fs'; fs.writeFileSync(${JSON.stringify(out)}, JSON.stringify({ argv: process.argv.slice(2), env: process.env }));\n`);
  return file;
}

/** A session environment carrying what sshd's AcceptEnv could let a client send. */
const hostile = (extra = {}) => ({
  PATH: process.env.PATH,
  HOME: os.homedir(),
  LANG: 'C.UTF-8',
  NODE_OPTIONS: '--no-warnings',
  NODE_PATH: '/tmp/elsewhere',
  NODE_EXTRA_CANARY: '1',
  LEGIONCTL_HOME: '/tmp/not-the-base',
  UNLISTED_CANARY: 'should not arrive',
  ...extra,
});

const shells = ['/bin/sh', '/bin/dash', '/bin/bash', '/bin/zsh', '/bin/ksh'].filter((shell) => fs.existsSync(shell));

test('the restricted forced command starts the agent with only the variables it lists', { skip: process.platform === 'win32' && 'POSIX shells only' }, () => {
  const bin = fs.mkdtempSync(path.join(os.tmpdir(), "dispatch env's "));
  try {
    const out = path.join(bin, 'seen.json');
    fs.copyFileSync(path.join(INSTALL, 'dispatch.sh'), path.join(bin, 'dispatch.sh'));
    // The recorder is the launcher itself, so no shell of its own adds variables after env -i.
    fs.writeFileSync(path.join(bin, 'legionctl'), `#!${process.execPath}\n${fs.readFileSync(recorder(bin, out), 'utf8')}`, { mode: 0o755 });
    // Everything a client may put in the command arrives as it was sent.
    const line = `node /x/index.mjs run 'a b' "$(id)" \`id\` ; * $HOME ~ \\ '\\''`;
    for (const shell of shells) {
      fs.rmSync(out, { force: true });
      const run = spawnSync(shell, [path.join(bin, 'dispatch.sh')], { env: hostile({ SSH_ORIGINAL_COMMAND: line, SSH_CONNECTION: '192.0.2.5 50000 192.0.2.40 22' }), encoding: 'utf8' });
      assert.equal(run.status, 0, `${shell}: ${run.stderr}`);
      const seen = JSON.parse(fs.readFileSync(out, 'utf8'));
      assert.deepEqual(seen.argv, ['dispatch'], shell);
      assert.equal(seen.env.SSH_ORIGINAL_COMMAND, line, `${shell} passes the command as one unexpanded value`);
      assert.equal(seen.env.SSH_CONNECTION, '192.0.2.5 50000 192.0.2.40 22');
      assert.equal(seen.env.LEGIONCTL_RESTRICTED, '1');
      assert.equal(seen.env.LEGIONCTL_CLIENT, 'restricted-ssh');
      for (const name of ['NODE_OPTIONS', 'NODE_PATH', 'NODE_EXTRA_CANARY', 'UNLISTED_CANARY', 'LEGIONCTL_HOME']) {
        assert.equal(name in seen.env, false, `${shell} let ${name} through`);
      }
      const listed = new Set(['PATH', 'HOME', 'USER', 'LOGNAME', 'SHELL', 'LANG', 'LC_ALL', 'LC_CTYPE', 'LC_MESSAGES', 'TZ', 'TMPDIR',
        'SSH_CONNECTION', 'SSH_CLIENT', 'SSH_ORIGINAL_COMMAND', 'XDG_RUNTIME_DIR', 'DBUS_SESSION_BUS_ADDRESS', 'LEGIONCTL_RESTRICTED', 'LEGIONCTL_CLIENT']);
      // A shell exports a few of its own (PWD, SHLVL), and env -i stops those
      // too. macOS gives every process its text encoding.
      if (process.platform === 'darwin') listed.add('__CF_USER_TEXT_ENCODING');
      assert.deepEqual(Object.keys(seen.env).filter((name) => !listed.has(name)), [], shell);
    }
  } finally { fs.rmSync(bin, { recursive: true, force: true }); }
});

test('the stable launcher drops NODE_OPTIONS and NODE_PATH before Node starts, and every NODE_* before the agent does', { skip: process.platform === 'win32' && 'POSIX shell wrapper' }, () => {
  const base = fs.mkdtempSync(path.join(os.tmpdir(), 'launcher env '));
  try {
    const bin = path.join(base, 'bin');
    fs.mkdirSync(bin);
    const out = path.join(base, 'seen.json');
    // The shell wrapper, rendered the way install-linux.sh renders it, in front of a recorder.
    fs.writeFileSync(path.join(bin, 'legionctl'), fs.readFileSync(path.join(INSTALL, 'launcher.sh'), 'utf8').replace('@NODE_SHELL@', quote(process.execPath)), { mode: 0o755 });
    fs.copyFileSync(recorder(base, out), path.join(bin, 'launcher.mjs'));
    const wrapped = spawnSync('/bin/sh', [path.join(bin, 'legionctl'), 'status'], { env: hostile(), encoding: 'utf8' });
    assert.equal(wrapped.status, 0, wrapped.stderr);
    const seen = JSON.parse(fs.readFileSync(out, 'utf8'));
    assert.deepEqual(seen.argv, ['status']);
    assert.equal('NODE_OPTIONS' in seen.env, false);
    assert.equal('NODE_PATH' in seen.env, false);
    assert.equal(seen.env.LEGIONCTL_HOME, fs.realpathSync(base));

    // The launcher itself, in front of an agent that records what it got.
    fs.copyFileSync(path.join(INSTALL, 'launcher.mjs'), path.join(bin, 'launcher.mjs'));
    fs.mkdirSync(path.join(base, 'agent', 'src'), { recursive: true });
    fs.copyFileSync(recorder(base, out), path.join(base, 'agent', 'src', 'index.mjs'));
    fs.writeFileSync(path.join(base, 'agent', 'package.json'), JSON.stringify({ type: 'module', version: '3.1.0' }));
    fs.rmSync(out);
    const launched = spawnSync(process.execPath, [path.join(bin, 'launcher.mjs'), 'status'], { env: { ...hostile(), KEPT_CANARY: 'kept' }, encoding: 'utf8' });
    assert.equal(launched.status, 0, launched.stderr);
    const agent = JSON.parse(fs.readFileSync(out, 'utf8'));
    assert.deepEqual(agent.argv, ['status']);
    assert.deepEqual(Object.keys(agent.env).filter((name) => name.toUpperCase().startsWith('NODE_')), []);
    assert.equal(agent.env.KEPT_CANARY, 'kept', 'everything else is the agent\'s as before');
  } finally { fs.rmSync(base, { recursive: true, force: true }); }
});

const powershell = process.platform === 'win32'
  ? ['powershell.exe', 'pwsh.exe'].find((name) => spawnSync(name, ['-NoProfile', '-Command', 'exit 0']).status === 0)
  : undefined;

test('the Windows forced command starts the agent with only the variables it lists', { skip: !powershell && 'PowerShell on Windows only' }, () => {
  const bin = fs.mkdtempSync(path.join(os.tmpdir(), 'dispatch env '));
  try {
    const out = path.join(bin, 'seen.json');
    fs.copyFileSync(path.join(INSTALL, 'dispatch.ps1'), path.join(bin, 'dispatch.ps1'));
    // legionctl.ps1 records what it was started with, as the rendered launcher would pass it on.
    fs.writeFileSync(path.join(bin, 'legionctl.ps1'), [
      '$names = @([Environment]::GetEnvironmentVariables(\'Process\').Keys | ForEach-Object { ([string] $_).ToUpperInvariant() })',
      '$seen = @{ names = $names; command = $env:SSH_ORIGINAL_COMMAND; restricted = $env:LEGIONCTL_RESTRICTED; arguments = @($args) }',
      `[IO.File]::WriteAllText(${JSON.stringify(out).replaceAll('"', "'")}, ($seen | ConvertTo-Json -Compress))`,
      'exit 0',
    ].join('\r\n'));
    const line = `node C:\\x\\index.mjs run 'a b' $(whoami) \`$env:PATH ; & | > %PATH%`;
    const run = spawnSync(powershell, ['-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-File', path.join(bin, 'dispatch.ps1')], {
      env: { ...process.env, NODE_OPTIONS: '--no-warnings', NODE_PATH: 'C:\\elsewhere', UNLISTED_CANARY: 'x', SSH_ORIGINAL_COMMAND: line },
      encoding: 'utf8',
    });
    assert.equal(run.status, 0, run.stdout + run.stderr);
    const seen = JSON.parse(fs.readFileSync(out, 'utf8').replace(/^\uFEFF/, ''));
    assert.equal(seen.command, line);
    assert.equal(seen.restricted, '1');
    assert.deepEqual([].concat(seen.arguments), ['dispatch']);
    for (const name of ['NODE_OPTIONS', 'NODE_PATH', 'UNLISTED_CANARY']) assert.equal(seen.names.includes(name), false, name);
    assert.ok(seen.names.includes('SYSTEMROOT'));
  } finally { fs.rmSync(bin, { recursive: true, force: true }); }
});
