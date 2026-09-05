// The restricted ssh dispatcher: the grammar, and what a restricted key may do.

import assert from 'node:assert/strict';
import path from 'node:path';
import fs from 'node:fs';
import os from 'node:os';
import test from 'node:test';
import { authorize, parseCommandLine, ownEntry, RESTRICTED_COMMANDS } from '../src/dispatch.mjs';

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

test('the allow list is the one published to the clients', () => {
  for (const command of ['status', 'busy', 'update', 'restart', 'boot', 'sleep', 'run', 'cycle', 'op', 'cancel', 'history', 'logs', 'doctor', 'bundle', 'config', 'policy', 'version']) {
    assert.ok(RESTRICTED_COMMANDS.has(command), `${command} should be allowed for a restricted key`);
  }
  assert.equal(RESTRICTED_COMMANDS.has('dispatch'), false);
  assert.equal(RESTRICTED_COMMANDS.has('op-run'), false);
});
