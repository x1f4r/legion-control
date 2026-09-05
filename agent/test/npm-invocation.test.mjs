import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { runCommand } from '../src/config.mjs';
import { npmInvocation } from '../src/providers/npm.mjs';
import { withHome } from './helpers.mjs';

function npmTree(directory) {
  fs.mkdirSync(path.join(directory, 'node_modules', 'npm', 'bin'), { recursive: true });
  fs.writeFileSync(path.join(directory, 'npm.cmd'), '@echo launcher must not run');
  const entry = path.join(directory, 'node_modules', 'npm', 'bin', 'npm-cli.js');
  fs.writeFileSync(entry, 'process.stdout.write(JSON.stringify(process.argv.slice(2)))');
  return entry;
}

test('Windows PATH npm is called through its JavaScript entry with literal prefix and package arguments', async () => withHome(async (home) => {
  const directory = path.join(home, 'Other Node & tools');
  const entry = npmTree(directory);
  const invocation = npmInvocation(null, { platform: 'win32', nodePath: path.join(home, 'bundled', 'node.exe'), searchPath: `"${directory}"` });
  assert.equal(invocation.shell, false);
  assert.deepEqual(invocation.lead, [entry]);
  const prefix = 'C:\\Users\\A & B\\npm prefix';
  const args = ['install', '--prefix', prefix, '@scope/package@1.0.0&echo unexpected', '%PATH%', 'a"b', 'tail\\'];
  const result = runCommand(process.execPath, [...invocation.lead, ...args], { shell: invocation.shell });
  assert.equal(result.ok, true, result.error);
  assert.deepEqual(JSON.parse(result.stdout), args, 'every argument must reach npm unchanged without shell interpretation');
}));

test('Windows npm launcher without an adjacent JavaScript entry fails closed', async () => withHome(async (home) => {
  fs.writeFileSync(path.join(home, 'npm.cmd'), '@echo unsafe fallback');
  const invocation = npmInvocation(null, { platform: 'win32', nodePath: path.join(home, 'bundled', 'node.exe'), searchPath: home });
  assert.equal(invocation.file, null);
  assert.equal(invocation.shell, false);
  assert.match(invocation.error, /npm-cli\.js could not be located/);
}));

test('Windows npm resolves a launcher symlink to its actual installation', { skip: process.platform === 'win32' }, async () => withHome(async (home) => {
  const actual = path.join(home, 'actual install');
  const entry = npmTree(actual);
  const shims = path.join(home, 'shims');
  fs.mkdirSync(shims);
  fs.symlinkSync(path.join(actual, 'npm.cmd'), path.join(shims, 'npm.cmd'));
  const invocation = npmInvocation(null, { platform: 'win32', nodePath: path.join(home, 'bundled', 'node.exe'), searchPath: shims });
  assert.deepEqual(invocation.lead, [fs.realpathSync(entry)]);
  assert.equal(invocation.shell, false);
}));
