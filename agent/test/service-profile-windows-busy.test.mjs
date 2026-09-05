import test from 'node:test';
import assert from 'node:assert/strict';
import { inspectProfileProcesses, processExitVerdict } from '../src/service-profile-probe.mjs';

const windows = { platform: 'win32', systemRoot: 'C:\\Windows' };
const verdict = (rows, options = windows) => processExitVerdict('codex-cli', rows, 99999, options);
const system = { pid: 200, parentPid: 100, sessionId: 0, name: 'svchost.exe', command: null, ownerSid: 'S-1-5-18' };

test('Windows reserved kernel identities require exact PID, parent and session evidence', () => {
  const rows = [
    { pid: 0, parentPid: 0, sessionId: 0, name: 'System Idle Process', command: null },
    { pid: 4, parentPid: 0, sessionId: 0, name: 'System', command: null },
  ];
  assert.equal(verdict(rows).busy, false);
  assert.equal(verdict(rows, { platform: 'linux' }).unknown, true);
  for (const replacement of [{ pid: null }, { pid: 400 }, { parentPid: 9 }, { sessionId: 1 }, { name: 'node.exe' }]) {
    assert.equal(verdict([{ ...rows[0], ...replacement }]).unknown, true);
  }
  assert.equal(verdict([{ ...rows[1], name: 'codex.exe' }]).unknown, false);
  assert.equal(verdict([{ ...rows[1], name: 'codex.exe' }]).busy, true);
});

test('protected Windows services need both a fixed OS name and a verified service owner', () => {
  for (const ownerSid of ['S-1-5-18', 'S-1-5-19', 'S-1-5-20']) assert.equal(verdict([{ ...system, ownerSid }]).busy, false);
  for (const replacement of [{ ownerSid: null }, { ownerSid: 'S-1-5-21-1-2-3-1001' }, { name: 'unknown-user.exe' }, { sessionId: 1 }, { parentPid: null }]) {
    assert.equal(verdict([{ ...system, ...replacement }]).unknown, true);
  }
  for (const name of ['node.exe', 'bun.exe', 'electron.exe']) assert.equal(verdict([{ ...system, name }]).unknown, true);
  for (const name of ['codex.exe', 'codex-cli.exe']) {
    const result = verdict([{ ...system, name }]);
    assert.equal(result.busy, true);
    assert.equal(result.unknown, false);
  }
  for (const name of ['Secure System', 'Registry', 'Memory Compression']) {
    assert.equal(verdict([{ ...system, name, parentPid: 4 }]).busy, false);
    assert.equal(verdict([{ ...system, name, parentPid: 4, ownerSid: 'S-1-5-19' }]).unknown, true);
    assert.equal(verdict([{ ...system, name }]).unknown, true);
  }
  assert.equal(verdict([{ ...system, name: 'csrss.exe', sessionId: 2 }]).busy, false);
});

test('WSL memory accounting requires a VM owner and the system VM worker in the snapshot', () => {
  const vm = { ...system, name: 'vmmemWSL', ownerSid: 'S-1-5-83-1-2-3-4-5' };
  const parent = { pid: 100, name: 'vmwp.exe', command: 'C:\\Windows\\System32\\vmwp.exe', executablePath: 'C:\\WINDOWS\\System32\\vmwp.exe' };
  assert.equal(verdict([vm, parent]).busy, false);
  for (const replacement of [{ ownerSid: 'S-1-5-21-1-2-3-4-5' }, { ownerSid: 'S-1-5-83-0' }, { sessionId: 1 }, { parentPid: 300 }, { name: 'node.exe' }]) {
    assert.equal(verdict([{ ...vm, ...replacement }, parent]).unknown, true);
  }
  assert.equal(verdict([vm, { ...parent, executablePath: 'C:\\Users\\someone\\vmwp.exe' }]).unknown, true);
  assert.equal(verdict([vm, { ...parent, executablePath: null }]).unknown, true);
});

test('Windows CIM collection carries owner evidence under a single bounded process deadline', () => {
  let calls = 0;
  const run = (file, args, options) => {
    calls++;
    assert.equal(file, 'powershell.exe');
    assert.equal(options.timeout, 4000);
    assert.equal(options.maxBuffer, 4 * 1024 * 1024);
    assert.match(args.at(-1), /Invoke-CimMethod.*GetOwnerSid/);
    return { status: 0, stdout: JSON.stringify([
      { ProcessId: 0, ParentProcessId: 0, SessionId: 0, Name: 'System Idle Process', CommandLine: null },
      { ProcessId: 4, ParentProcessId: 0, SessionId: 0, Name: 'System', CommandLine: null },
      { ProcessId: 200, ParentProcessId: 100, SessionId: 0, Name: 'svchost.exe', CommandLine: null, OwnerSid: 'S-1-5-18' },
    ]) };
  };
  assert.equal(inspectProfileProcesses('codex-cli', { platform: 'win32', run }).busy, false);
  assert.equal(calls, 1);
  const missingOwner = () => ({ status: 0, stdout: JSON.stringify([{ ProcessId: 200, ParentProcessId: 100, SessionId: 0, Name: 'svchost.exe', CommandLine: null }]) });
  assert.equal(inspectProfileProcesses('codex-cli', { platform: 'win32', run: missingOwner }).unknown, true);
  const malformedPid = () => ({ status: 0, stdout: JSON.stringify([{ ProcessId: null, ParentProcessId: 0, SessionId: 0, Name: 'System Idle Process', CommandLine: null }]) });
  assert.equal(inspectProfileProcesses('codex-cli', { platform: 'win32', run: malformedPid }).unknown, true);
});

test('Windows process failures remain unknown and never disclose query output or command lines', () => {
  for (const response of [{ status: null, error: new Error('opaque-secret'), stdout: 'opaque-secret' }, { status: 1, stderr: 'opaque-secret' }, { status: 0, stdout: '{opaque-secret' }]) {
    const result = inspectProfileProcesses('codex-cli', { platform: 'win32', run: () => response });
    assert.equal(result.unknown, true);
    assert.doesNotMatch(JSON.stringify(result), /opaque-secret/);
  }
  const result = verdict([{ pid: 100, name: 'codex.exe', command: 'codex.exe opaque-secret' }, system]);
  assert.equal(result.busy, true);
  assert.equal(result.unknown, false);
  assert.doesNotMatch(JSON.stringify(result), /opaque-secret/);
});
