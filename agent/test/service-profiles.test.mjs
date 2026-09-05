import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { discoverServiceProfiles } from '../src/service-profiles.mjs';
import { inspectProfileProcesses, processExitVerdict, latestClaudeVersion, readProfileVersion, runNativeClaudeProbe } from '../src/service-profile-probe.mjs';
import { resolveClaudeUpdatePolicy, claudeTargetAllowed, readClaudeRemotePolicy } from '../src/service-profile-claude.mjs';
import { normalizeConfig } from '../src/config.mjs';
import { readDesktopMetadata } from '../src/service-profile-desktop.mjs';

// These fixtures describe a simulated POSIX host even when tests run on
// Windows. Match filesystem lookups by separators, while keeping actual argv
// assertions in the test runner's native path form.
const fixturePath = (file) => String(file).replaceAll('\\', '/');

function mockFiles(files = {}) {
  files = { '/proc/version': 'Linux version 6.12.0', ...files };
  const missing = () => Object.assign(new Error('missing'), { code: 'ENOENT' });
  return {
    statSync(file) {
      file = fixturePath(file);
      if (Object.hasOwn(files, file)) return { isFile: () => true, size: Buffer.byteLength(files[file]) };
      if (Object.keys(files).some((entry) => entry.startsWith(`${file}/`))) return { isFile: () => false, size: 0 };
      throw missing();
    },
    readFileSync(file) { file = fixturePath(file); if (!Object.hasOwn(files, file)) throw missing(); return files[file]; },
    readdirSync(directory) { directory = fixturePath(directory); return [...new Set(Object.keys(files).filter((file) => file.startsWith(`${directory}/`)).map((file) => file.slice(directory.length + 1).split('/')[0]))]; },
  };
}
const policyOptions = (files = {}, env = {}) => ({ home: '/home/test', platform: 'linux', executable: '/mock/claude', inspectRemote: () => 'not-eligible', env: { ANTHROPIC_BASE_URL: 'https://custom-provider.example', ...env }, io: mockFiles(files) });

test('native policy resolves local/managed precedence and blocks unsupported or disabled updates', () => {
  const user = '/home/test/.claude/settings.json';
  const local = '/home/test/.claude/settings.local.json';
  const managed = '/etc/claude-code/managed-settings.json';
  const files = { [user]: JSON.stringify({ autoUpdatesChannel: 'stable', env: { DISABLE_UPDATES: 'false', SECRET: 'never-return' } }), [local]: JSON.stringify({ autoUpdatesChannel: 'latest' }) };
  const resolved = resolveClaudeUpdatePolicy(policyOptions(files, { DISABLE_UPDATES: 'TRUE' }));
  assert.equal(resolved.ok, true); assert.equal(resolved.channel, 'latest'); assert.doesNotMatch(JSON.stringify(resolved), /never-return/);
  const managedValue = resolveClaudeUpdatePolicy(policyOptions({ ...files, [managed]: JSON.stringify({ autoUpdatesChannel: 'stable', minimumVersion: '2.1.240' }) }));
  assert.equal(managedValue.channel, 'stable'); assert.equal(managedValue.minimumVersion, '2.1.240');
  assert.equal(claudeTargetAllowed(managedValue, '2.1.236'), false);
  assert.equal(claudeTargetAllowed(managedValue, '2.1.260', '2.1.261'), false);
  assert.equal(claudeTargetAllowed(managedValue, '2.1.261', '2.1.261'), true);
  assert.notEqual(resolved.hash, managedValue.hash);
  for (const setting of [{ policyHelper: '/must/not/execute' }, { env: { DISABLE_UPDATES: '1' } }, { autoUpdatesChannel: 'unknown' }, { env: { CLAUDE_CONFIG_DIR: '/redirected' } }, { managedSourcesBehavior: 'unknown' }]) {
    assert.equal(resolveClaudeUpdatePolicy(policyOptions({ [managed]: JSON.stringify(setting) })).ok, false);
  }
});

test('native policy refuses unknown remote approval, registry policy and detected mac preference policy', () => {
  assert.equal(resolveClaudeUpdatePolicy(policyOptions({ '/home/test/.claude/remote-settings.json': '{"autoUpdatesChannel":"stable"}' }, { ANTHROPIC_BASE_URL: '' })).ok, false);
  assert.equal(resolveClaudeUpdatePolicy({ ...policyOptions(), inspectRemote: () => null }).ok, false);
  assert.equal(resolveClaudeUpdatePolicy({ ...policyOptions(), platform: 'win32' }).ok, false);
  assert.equal(resolveClaudeUpdatePolicy(policyOptions({ '/proc/version': 'Linux Microsoft WSL2' })).ok, false);
  assert.equal(resolveClaudeUpdatePolicy({ ...policyOptions(), platform: 'darwin', io: mockFiles({ '/Library/Managed Preferences/com.anthropic.claudecode.plist': 'policy' }) }).ok, false);
  assert.equal(resolveClaudeUpdatePolicy({ ...policyOptions(), username: 'different-account', platform: 'darwin', io: mockFiles({ '/Library/Managed Preferences/different-account/com.anthropic.claudecode.plist': 'policy' }) }).ok, false);
  const bounds = resolveClaudeUpdatePolicy(policyOptions({ '/etc/claude-code/managed-settings.json': '{"requiredMinimumVersion":"2.1.200","requiredMaximumVersion":"2.1.250"}' }));
  assert.equal(claudeTargetAllowed(bounds, '2.1.199'), false); assert.equal(claudeTargetAllowed(bounds, '2.1.251'), false);
  const ignored = resolveClaudeUpdatePolicy(policyOptions({ '/home/test/.claude/settings.json': '{"requiredMinimumVersion":"invalid","requiredMaximumVersion":"1.0.0"}' }));
  assert.equal(ignored.ok, true); assert.equal(ignored.requiredMaximumVersion, null);
  assert.equal(claudeTargetAllowed(bounds, '2.1.240', 'unknown'), false);
});

test('publisher latest data is bounded, validated and never guessed from version output', async () => {
  const request = async (url) => { assert.equal(url, 'https://downloads.claude.ai/claude-code-releases/stable'); return new Response('2.1.236\n'); };
  assert.equal(await latestClaudeVersion('stable', { request }), '2.1.236');
  assert.equal(await latestClaudeVersion('other', { request }), null);
  assert.equal(await latestClaudeVersion('stable', { request: async () => new Response('unknown') }), null);
  assert.equal(await latestClaudeVersion('stable', { request: async () => new Response('x'.repeat(5000)) }), null);
  assert.equal(readProfileVersion('/mock/claude', { run: () => ({ status: 0, stdout: '2.1.261 (Claude Code)\n' }) }), '2.1.261');
  assert.equal(readProfileVersion('/mock/claude', { run: () => ({ status: 0, stdout: 'unknown' }) }), null);
});

test('native remote inspection returns only an explicit no-policy diagnostic', () => {
  const inspect = (stdout) => readClaudeRemotePolicy('/mock/claude', { home: '/home/test', run: (executable, argv, options) => {
    assert.equal(executable, '/mock/claude'); assert.deepEqual(argv, ['doctor']); assert.equal(options.cwd, '/home/test');
    return { status: 0, stdout };
  } });
  assert.equal(inspect('Private account details\nManaged settings (remote): not fetched — requires an Enterprise or Team subscription\n'), 'not-eligible');
  assert.equal(inspect('Managed settings (remote): loaded'), null);
  assert.equal(inspect('Managed settings (remote): not fetched yet'), null);
  assert.equal(resolveClaudeUpdatePolicy({ ...policyOptions(), inspectRemote: () => null }).ok, false);
});

test('native update rechecks policy, fixes cwd to home and never exposes updater output', async () => {
  const policy = resolveClaudeUpdatePolicy(policyOptions());
  let calls = 0;
  const options = { executable: '/mock/claude', home: '/home/test', expectedPolicy: policy.hash };
  for (const owned of [null, '2.1.240']) {
    const unsupported = await runNativeClaudeProbe('native-update', options, { nativeVersion: () => owned, run: () => { throw new Error('must not execute'); }, resolvePolicy: () => { throw new Error('must not inspect an unsupported product'); } });
    assert.equal(unsupported.ok, false);
  }
  const run = (executable, args, options) => {
    calls += 1; assert.equal(executable, '/mock/claude'); assert.equal(options.cwd, '/home/test');
    if (args[0] === '--version') return { status: 0, stdout: '2.1.261 (Claude Code)' };
    assert.deepEqual(args, ['update']); return { status: 0, stdout: 'private-output', stderr: 'private-output' };
  };
  const refused = await runNativeClaudeProbe('native-update', options, { resolvePolicy: () => ({ ...policy, hash: 'changed' }), nativeVersion: () => '2.1.261', run });
  assert.equal(refused.ok, false); assert.equal(calls, 0);
  const updated = await runNativeClaudeProbe('native-update', options, { resolvePolicy: () => policy, nativeVersion: () => '2.1.261', run, latest: async () => '2.1.261' });
  assert.deepEqual(updated, { ok: true }); assert.equal(calls, 3);
  const mismatch = await runNativeClaudeProbe('native-update', options, { resolvePolicy: () => policy, nativeVersion: () => '2.1.261', run, latest: async () => '2.1.262' });
  assert.equal(mismatch.ok, false);
  const latest = await runNativeClaudeProbe('native-latest', options, { resolvePolicy: () => policy, nativeVersion: () => '2.1.261', run, latest: async () => '2.1.262' });
  assert.equal(latest.version, '2.1.262');
  const noDowngrade = await runNativeClaudeProbe('native-latest', options, { resolvePolicy: () => policy, nativeVersion: () => '2.1.261', run, latest: async () => '2.1.260' });
  assert.equal(noDowngrade.ok, false);
  const mismatchedBinary = await runNativeClaudeProbe('native-update', options, { resolvePolicy: () => policy, nativeVersion: () => '2.1.261', run: (_file, args) => { assert.deepEqual(args, ['--version']); return { status: 0, stdout: '2.1.240' }; }, latest: async () => '2.1.262' });
  assert.equal(mismatchedBinary.ok, false);
});

test('profile process protection covers running sessions and fails closed without evidence', () => {
  assert.equal(processExitVerdict('claude-code', [{ pid: 2, name: 'claude', command: 'claude --token private-value' }], 100).busy, true);
  assert.equal(processExitVerdict('claude-code', [{ pid: 2, name: 'node', command: 'node /prefix/node_modules/@anthropic-ai/claude-code/cli.js' }], 100).busy, true);
  assert.equal(processExitVerdict('codex-cli', [{ pid: 2, name: 'node', command: 'node /prefix/node_modules/@openai/codex/bin/codex.js' }], 100).busy, true);
  assert.equal(processExitVerdict('opencode', [{ pid: 2, name: 'opencode.exe', command: 'opencode.exe' }], 100).busy, true);
  for (const id of ['claude-code', 'codex-cli', 'opencode']) {
    assert.equal(processExitVerdict(id, [{ pid: 2, name: 'node', command: `node /opt/legion/agent/src/index.mjs update --service ${id}` }], 100).busy, false, 'the invoking agent is not a product session');
  }
  assert.equal(processExitVerdict('codex-cli', [{ pid: 2, name: 'node.exe', command: null }], 100).unknown, true);
  assert.equal(processExitVerdict('claude-code', [{ pid: 2, name: '/bin/bash', command: '/bin/bash' }], 100).busy, false);
  assert.equal(inspectProfileProcesses('codex-cli', { platform: 'linux', run: () => ({ status: 1, stdout: 'private-value' }) }).unknown, true);
  for (const binary of ['/tmp/custom runtime/node', '/tmp/custom runtime tools/node']) {
    assert.equal(inspectProfileProcesses('claude-code', { platform: 'darwin', run: (_file, args) => { assert.equal(args[0], '-ww'); return { status: 0, stdout: args.includes('comm=') ? `123 ${binary}\n` : `123 ${binary} /prefix/node_modules/@anthropic-ai/claude-code/cli.js\n` }; } }).busy, true);
    assert.equal(processExitVerdict('claude-code', [{ pid: 123, name: binary, command: `${binary} /opt/legion/agent/src/index.mjs update --service claude-code` }]).busy, false);
  }
  assert.doesNotMatch(JSON.stringify(inspectProfileProcesses('codex-cli', { platform: 'linux', run: () => ({ status: 1, stdout: 'private-value' }) })), /private-value/);
});

test('desktop monitoring reads metadata through fixed OS tools and never starts the application', () => {
  const location = '/Applications/Claude.app';
  const io = mockFiles({ [`${location}/Contents/Info.plist`]: 'binary-plist' });
  const run = (executable, argv) => {
    assert.equal(executable, '/usr/bin/plutil');
    assert.deepEqual(argv, ['-convert', 'json', '-o', '-', path.join(location, 'Contents', 'Info.plist')]);
    return { status: 0, stdout: JSON.stringify({ CFBundleShortVersionString: '1.46388.4', CFBundleIdentifier: 'com.anthropic.claudefordesktop', Unrelated: 'never-return' }) };
  };
  assert.deepEqual(readDesktopMetadata(location, { platform: 'darwin', io, run }), { version: '1.46388.4', identity: 'com.anthropic.claudefordesktop' });
  assert.equal(readDesktopMetadata(location, { platform: 'darwin', io, run: () => ({ status: 0, stdout: '{"CFBundleShortVersionString":"unknown"}' }) }), null);
  const windowsLocation = path.resolve("app's folder", 'Claude.exe');
  const windows = readDesktopMetadata(windowsLocation, { platform: 'win32', io: { statSync: () => ({ isFile: () => true }) }, run: (executable, argv, options) => {
    assert.equal(executable, 'powershell.exe'); assert.equal(options.env.LEGION_PROFILE_EXECUTABLE, windowsLocation);
    assert.equal(argv.some((arg) => arg.includes(windowsLocation)), false);
    return { status: 0, stdout: '{"version":"1.2.3","identity":"Claude"}' };
  } });
  assert.deepEqual(windows, { version: '1.2.3', identity: 'Claude' });
  const desktopIo = { ...io, statSync: (file) => fixturePath(file) === location ? { isDirectory: () => true } : io.statSync(file) };
  const profiles = discoverServiceProfiles({ platform: 'mac', home: '/home/test', env: {}, io: desktopIo, readDesktop: (file, options) => readDesktopMetadata(file, { ...options, run }) });
  const profile = profiles.find((entry) => entry.id === 'claude-desktop');
  assert.equal(profile.availability, 'manual'); assert.equal(profile.service.updates.automatic, false);
  assert.equal(profile.service.update, undefined); assert.equal(profile.service.latestVersion, undefined);
  assert.equal(normalizeConfig({ configVersion: 3, services: [profile.service] }).ok, true);
});

test('npm profile discovery preserves prefix and known channel without executing anything', { skip: process.platform === 'win32' }, () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'legion profiles '));
  try {
    const prefix = path.join(root, 'prefix'); const bin = path.join(prefix, 'bin'); fs.mkdirSync(bin, { recursive: true });
    const marker = path.join(root, 'must-not-execute');
    fs.writeFileSync(path.join(bin, 'npm'), `#!/bin/sh\ntouch '${marker}'\n`, { mode: 0o755 });
    for (const [exe, name, version] of [['codex', '@openai/codex', '0.153.4'], ['opencode', 'opencode-ai', '1.18.29']]) {
      const packageRoot = path.join(prefix, 'lib/node_modules', name); fs.mkdirSync(path.join(packageRoot, 'bin'), { recursive: true });
      fs.writeFileSync(path.join(packageRoot, 'package.json'), JSON.stringify({ name, version }));
      fs.writeFileSync(path.join(packageRoot, 'bin', exe), `#!/bin/sh\ntouch '${marker}'\n`, { mode: 0o755 });
      fs.symlinkSync(path.join(packageRoot, 'bin', exe), path.join(bin, exe));
    }
    const profiles = discoverServiceProfiles({ platform: 'linux', home: root, env: { PATH: bin }, configuration: { services: [{ kind: 'npm', package: '@openai/codex', npmPrefix: prefix, channel: 'alpha' }] } });
    const codex = profiles.find((profile) => profile.id === 'codex-cli');
    assert.equal(codex.availability, 'available'); assert.equal(codex.service.npmPrefix, fs.realpathSync(prefix)); assert.equal(codex.service.channel, 'alpha'); assert.equal(codex.service.updates.automatic, false);
    assert.deepEqual(profiles.find((profile) => profile.id === 'opencode').service.allowScripts, ['opencode-ai']);
    assert.equal(profiles.filter((profile) => ['codex-desktop', 'chatgpt-desktop'].includes(profile.id)).length, 1);
    assert.equal(profiles.find((profile) => profile.id === 'grok-cli').availability, 'unavailable');
    assert.equal(fs.existsSync(marker), false);
    assert.equal(normalizeConfig({ configVersion: 3, services: [codex.service] }).ok, true);
    const separatePrefix = discoverServiceProfiles({ platform: 'linux', home: root, env: { PATH: bin }, configuration: { services: [{ kind: 'npm', package: '@openai/codex', npmPrefix: path.join(root, 'another-install'), channel: 'alpha' }] } }).find((profile) => profile.id === 'codex-cli');
    assert.equal(separatePrefix.service.channel, 'latest');
    const unrelated = path.join(root, 'native-codex'); fs.writeFileSync(unrelated, '#!/bin/sh\nexit 99\n', { mode: 0o755 });
    fs.unlinkSync(path.join(bin, 'codex')); fs.symlinkSync(unrelated, path.join(bin, 'codex'));
    const native = discoverServiceProfiles({ platform: 'linux', home: root, env: { PATH: bin } }).find((profile) => profile.id === 'codex-cli');
    assert.equal(native.availability, 'manual'); assert.equal(native.service, null, 'nearby stale npm metadata cannot prove executable ownership');
  } finally { fs.rmSync(root, { recursive: true, force: true }); }
});
