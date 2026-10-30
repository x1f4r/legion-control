// What other accounts on the machine can read of the agent's files (T8).
//
// The base holds every configured command, the setup document, the log and
// the record of every operation. They used to be written with whatever the
// umask allowed, and an atomic rename undid any chmod a person had made.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { broadAclGroups, exposedFiles, privateFilesCheck, sshShellCheck } from '../src/doctor.mjs';
import { cli, fixtureConfig, fixtureData, withHome, writeConfig } from './helpers.mjs';

const posix = { skip: process.platform === 'win32' && 'POSIX modes only' };
const modeOf = (file) => fs.statSync(file).mode & 0o777;

test('the base is 0700 and every file the agent writes is 0600, whatever the umask and an older install left', posix, async () => {
  await withHome(async (home) => {
    // What an older agent or an installer under umask 022 left behind.
    fs.chmodSync(home, 0o755);
    writeConfig(home, fixtureConfig(fixtureData(home)));
    fs.chmodSync(path.join(home, 'config.json'), 0o644);
    assert.equal(modeOf(path.join(home, 'config.json')), 0o644);

    assert.equal(cli(['policy', 'set'], { home, stdin: '{"automatic": false}' }).payload.ok, true);
    assert.equal(cli(['run', 'missing-action'], { home }).payload.ok, false);
    const setup = `${JSON.stringify({ version: 1, controller: { id: 'setup-private', revision: 1, lineage: [] }, machines: [] })}\n`;
    assert.equal(cli(['config', 'set'], { home, stdin: setup }).payload.action, 'stored');
    assert.equal(cli(['restart', '--service', 'demo'], { home }).payload.ok, true);

    assert.equal(modeOf(home), 0o700, 'the base');
    for (const name of ['config.json', 'config.last-good.json', 'controller.json', 'controller.meta.json', 'legionctl.log']) {
      assert.equal(modeOf(path.join(home, name)), 0o600, name);
    }
    for (const dir of ['ops', 'state']) {
      assert.equal(modeOf(path.join(home, dir)), 0o700, dir);
      const records = fs.readdirSync(path.join(home, dir)).filter((name) => name.endsWith('.json'));
      assert.ok(records.length > 0, `${dir} has records`);
      for (const name of records) assert.equal(modeOf(path.join(home, dir, name)), 0o600, `${dir}/${name}`);
    }
    assert.deepEqual(exposedFiles(home), []);
  });
});

test('a rewrite puts a private file in place even over one somebody opened up', posix, async () => {
  await withHome(async (home) => {
    writeConfig(home, fixtureConfig(fixtureData(home)));
    assert.equal(cli(['policy', 'set'], { home, stdin: '{"automatic": false}' }).payload.ok, true);
    fs.chmodSync(path.join(home, 'config.json'), 0o666);
    fs.chmodSync(path.join(home, 'state'), 0o777);
    assert.equal(cli(['policy', 'set'], { home, stdin: '{"automatic": true}' }).payload.ok, true);
    assert.equal(modeOf(path.join(home, 'config.json')), 0o600);
    assert.equal(cli(['restart', '--service', 'demo'], { home }).payload.ok, true);
    assert.equal(modeOf(path.join(home, 'state')), 0o700, 'a directory the agent writes into is narrowed');
  });
});

test('doctor says which of the agent\'s files other users can read, and how to stop it', posix, async () => {
  await withHome(async (home) => {
    fs.chmodSync(home, 0o755);
    writeConfig(home, fixtureConfig(fixtureData(home)));
    fs.chmodSync(path.join(home, 'config.json'), 0o644);
    const exposed = (await import('../src/doctor.mjs')).exposedFiles(home);
    assert.deepEqual(exposed, [{ path: home, mode: '755' }, { path: path.join(home, 'config.json'), mode: '644' }]);

    // Doctor reads; it never narrows anything itself.
    const check = cli(['doctor'], { home }).payload.checks.find((entry) => entry.id === 'files.private');
    assert.equal(check.level, 'warn');
    assert.match(check.summary, /^2 of the agent's directories and files can be read by other users$/);
    assert.match(check.detail, /config\.json \(644\)/);
    assert.equal(check.fix, `chmod -R go-rwx "${home}"`);

    fs.chmodSync(home, 0o700);
    fs.chmodSync(path.join(home, 'config.json'), 0o600);
    assert.equal(cli(['doctor'], { home }).payload.checks.find((entry) => entry.id === 'files.private').level, 'ok');
  });
});

test('on Windows doctor reads the base ACL by SID, in every language', () => {
  assert.deepEqual(broadAclGroups('O:S-1-5-21-1-2-3-1001G:S-1-5-21-1-2-3-513D:PAI(A;OICI;FA;;;S-1-5-21-1-2-3-1001)(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)'), []);
  assert.deepEqual(broadAclGroups('O:BAG:SYD:AI(A;OICIID;FA;;;SY)(A;OICIID;0x1200a9;;;BU)(A;OICIID;FA;;;AU)(D;;FA;;;WD)S:AI(AU;SA;FA;;;WD)'), ['Users', 'Authenticated Users']);
  assert.deepEqual(broadAclGroups('D:(A;;FA;;;S-1-1-0)'), ['Everyone']);

  const base = 'C:\\Users\\me\\.legion-control';
  const asked = [];
  const run = (file, args, options) => {
    asked.push({ file, args, path: options.env.LEGIONCTL_DOCTOR_PATH });
    return { ok: true, code: 0, stdout: 'O:BAG:SYD:AI(A;OICIID;FA;;;SY)(A;OICIID;0x1200a9;;;BU)\r\n', stderr: '', error: null, command: 'powershell.exe' };
  };
  const open = privateFilesCheck({ platform: 'win32', base, run });
  assert.equal(open.level, 'warn');
  assert.equal(open.summary, `Users may read ${base}`);
  assert.match(open.fix, /install-windows\.ps1/);
  // The path travels in the environment, never inside the script text.
  assert.equal(asked[0].path, base);
  assert.equal(asked[0].args.join(' ').includes(base), false);
  // And it reads the list through .NET, so no module has to be found first.
  assert.match(asked[0].args.at(-1), /\[System\.IO\.Directory\]::GetAccessControl/);
  assert.equal(asked[0].args.at(-1).includes('Get-Acl'), false);
  const closed = privateFilesCheck({ platform: 'win32', base, run: () => ({ ok: true, stdout: 'O:S-1-5-21-9G:S-1-5-21-9D:P(A;OICI;FA;;;S-1-5-21-9)', stderr: '', error: null }) });
  assert.equal(closed.level, 'ok');
  const failed = privateFilesCheck({ platform: 'win32', base, run: () => ({ ok: false, code: 1, stdout: '', stderr: 'Get-Acl: denied', error: null, command: 'powershell.exe', timedOut: false }) });
  assert.equal(failed.level, 'warn');
});

test('on Windows doctor says which shell OpenSSH runs', () => {
  const reg = (stdout, extra = {}) => sshShellCheck({ ok: stdout !== '', code: stdout ? 0 : 1, stdout, stderr: '', error: null, ...extra });
  const absent = reg('');
  assert.equal(absent.level, 'ok');
  assert.match(absent.summary, /cmd\.exe, the default/);
  assert.equal(reg('\r\nHKEY_LOCAL_MACHINE\\SOFTWARE\\OpenSSH\r\n    DefaultShell    REG_SZ    C:\\Windows\\System32\\cmd.exe\r\n\r\n').level, 'ok');
  const powershell = reg('\r\nHKEY_LOCAL_MACHINE\\SOFTWARE\\OpenSSH\r\n    DefaultShell    REG_SZ    C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe\r\n');
  assert.equal(powershell.level, 'ok');
  assert.match(powershell.detail, /set "shell": "powershell"/);
  assert.match(reg('    DefaultShell    REG_EXPAND_SZ    C:\\Program Files\\PowerShell\\7\\pwsh.exe\r\n').detail, /"shell": "powershell"/);
  const other = reg('    DefaultShell    REG_SZ    C:\\Program Files\\Git\\bin\\bash.exe\r\n');
  assert.equal(other.level, 'warn');
  assert.match(other.summary, /bash\.exe/);
  assert.equal(reg('', { error: 'spawn reg ENOENT' }).level, 'warn');
});
