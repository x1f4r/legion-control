import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawn, spawnSync } from 'node:child_process';
import test from 'node:test';
import { DatabaseSync } from 'node:sqlite';
import { checkoutFiles } from '../scripts/checkout-files.mjs';

const repository = path.resolve(import.meta.dirname, '..');
// Without carriage returns, so a checkout with Windows line endings reads the same.
const installer = fs.readFileSync(path.join(repository, 'agent', 'install', 'install-windows.ps1'), 'utf8').replaceAll('\r\n', '\n');
const linuxInstaller = fs.readFileSync(path.join(repository, 'agent', 'install', 'install-linux.sh'), 'utf8').replaceAll('\r\n', '\n');
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

// The principal rule, read off the script: what can be checked without Windows. The real runs below
// check what it does.
test('the Windows installer judges who the task runs as before it changes anything, and leaves no task XML behind', () => {
  assert.match(installer, /\[switch\] \$AllowSystemPrincipal/);
  const judged = installer.indexOf("Step 'Principal'");
  const protectedBase = installer.indexOf("Step 'Base'");
  const agent = installer.indexOf("Step 'Agent'");
  assert.ok(judged > 0 && protectedBase > judged && agent > protectedBase, 'principal, then base, then agent');
  // Every principal but the installing user's is refused, and SYSTEM is kept only with the flag.
  assert.match(installer, /if \(\$sid -eq \$UserSid\) \{[\s\S]*?\} elseif \(\$sid -eq \$SystemSid -and \$AllowSystemPrincipal\) \{[\s\S]*?\} elseif \(\$sid -eq \$SystemSid\) \{\s*throw[\s\S]*?\} else \{\s*throw/);
  // A new task is created as the installing user.
  assert.match(installer, /New-ScheduledTaskPrincipal -UserId \$identity\.Name -LogonType S4U/);
  // The XML schtasks imports is removed however the import went, and nothing is left for later.
  assert.match(installer, /\$doc\.Save\(\$xmlPath\)\s*try \{[^}]*schtasks\.exe[^}]*\} finally \{\s*Remove-Item -LiteralPath \$xmlPath/);
  assert.doesNotMatch(installer, /ONE STEP LEFT|RegisterCommand|exit 2/);
});

test('the Windows task gets the cycle budget as its time limit, new or rewritten', () => {
  assert.match(installer, /\$TimeLimit\s+= 'PT30M'/);
  assert.match(installer, /\$TimeLimitSpan = New-TimeSpan -Minutes 30/);
  assert.match(installer, /-ExecutionTimeLimit \$TimeLimitSpan/);
  assert.match(installer, /\$limitNode\.InnerText = \$TimeLimit/);
  // The same 30 minutes the systemd unit gives one cycle.
  assert.match(service, /^TimeoutStartSec=1800$/m);
});

test('the Windows installer gives the base to the task\'s account alone', () => {
  assert.match(installer, /Protect-Base -Sids @\(\$UserSid\)/);
  assert.match(installer, /Protect-Base -Sids @\(\$AdministratorsSid, \$SystemSid\) -Owner \$AdministratorsSid/);
  assert.match(installer, /'\/inheritance:r', '\/grant:r'/);
  assert.match(installer, /'\*' \+ \$_ \+ ':\(OI\)\(CI\)F'/);
});

// What sshd hands a command to decides how the clients quote it, so the installer says, as the
// agent's doctor does.
test('the Windows installer says which shell sshd runs commands with', () => {
  assert.match(installer, /Get-ItemProperty -LiteralPath 'HKLM:\\SOFTWARE\\OpenSSH' -Name DefaultShell/);
  assert.match(installer, /set "shell": "powershell" for this system in the setup/);
});

// The superseded updater one machine once ran is nothing a public installer should know about.
test('the Windows installer names no updater from before the agent', () => {
  assert.doesNotMatch(installer, /update-nightly|T3Code|AppData\\Local/i);
});

// --- The code both installers remove the agent's files with ------------------------

const removalCode = (text, start, end) => {
  const from = text.indexOf(start);
  assert.ok(from >= 0, start);
  return text.slice(from + start.length, text.indexOf(end, from + start.length));
};
const windowsRemoval = () => removalCode(installer, "$RemoveAgentFiles = @'\n", "\n'@\n");
const linuxRemoval = () => removalCode(linuxInstaller, `"$NODE_BIN" --input-type=module - "$BASE" "$1" <<'NODE'\n`, '\nNODE\n');
const runRemoval = (base, scope) => spawnSync(process.execPath, ['--input-type=module', '-', base, scope], { input: windowsRemoval(), encoding: 'utf8' });

test('both installers remove the agent\'s files with the same code', () => {
  assert.equal(windowsRemoval(), linuxRemoval());
});

// That code, run here the way the installers run it: on Windows CI as well, where it is the only
// way to see what -Uninstall removes without a scheduled task.
test('uninstalling takes the agent\'s code, purging its records too, and nothing that is not the agent\'s', (t) => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'legion removal '));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const base = path.join(root, "home with ' apostrophe", '.legion-control');
  const run = (scope) => runRemoval(base, scope);
  const code = ['bin', 'agent', 'agent.prev', 'agent.new', 'agent.install.new.1234', 'agent-swap.json', 'agent-baseline.json'];
  const records = ['config.json', 'config.last-good.json', 'controller.json', 'controller.meta.json', 'agent-floor.json', 'legionctl.log', 'legionctl.log.1', 'state', 'ops', 'incoming'];
  const theirs = ['private', 'release-signing.key', 'notes.txt'];
  fs.mkdirSync(base, { recursive: true });
  for (const name of [...code, ...records, ...theirs]) {
    if (/\.(?:json|log|1|key|txt)$/.test(name)) fs.writeFileSync(path.join(base, name), `${name}\n`);
    else fs.mkdirSync(path.join(base, name, 'inside'), { recursive: true });
  }

  // An operation that holds the lock keeps everything where it is.
  const db = new DatabaseSync(path.join(base, 'op.lock.sqlite'));
  db.exec('BEGIN IMMEDIATE');
  try {
    const locked = run('purge');
    assert.equal(locked.status, 1);
    assert.equal(locked.stderr.trim(), 'another operation is running; nothing was removed');
    assert.equal(locked.stdout, '');
  } finally { db.exec('ROLLBACK'); db.close(); }
  for (const name of [...code, ...records, ...theirs]) assert.equal(fs.existsSync(path.join(base, name)), true, name);

  const uninstalled = run('code');
  assert.equal(uninstalled.status, 0, uninstalled.stderr);
  assert.deepEqual(uninstalled.stdout.trim().split('\n'), [...code].sort().map((name) => `removed ${path.join(base, name)}`));
  assert.deepEqual(fs.readdirSync(base).sort(), [...records, ...theirs, 'op.lock.sqlite'].sort());

  const purged = run('purge');
  assert.equal(purged.status, 0, purged.stderr);
  assert.deepEqual(fs.readdirSync(base).sort(), [...theirs].sort());
  assert.ok(purged.stdout.split('\n').includes(`kept ${path.join(base, 'private')}, which is not the agent's`), purged.stdout);

  for (const name of theirs) fs.rmSync(path.join(base, name), { recursive: true });
  const emptied = run('purge');
  assert.equal(emptied.status, 0, emptied.stderr);
  assert.equal(emptied.stdout, `removed ${base}\n`);
  assert.equal(fs.existsSync(base), false);
  // And there is nothing to do once it is gone.
  const again = run('purge');
  assert.equal(again.status, 0, again.stderr);
  assert.equal(again.stdout, '');
});

// On Windows a folder a process works in cannot be moved, and elsewhere a folder that may not be
// written to cannot be moved into another, whose way back it would have to rewrite. bin sorts last,
// so every other entry has been moved aside by the time it stops the removal.
test('an entry that cannot be moved aside stops uninstalling with everything of the agent\'s where it was', { skip: process.getuid?.() === 0 }, async (t) => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'legion removal held '));
  const base = path.join(root, '.legion-control');
  const bin = path.join(base, 'bin');
  const code = ['agent', 'agent.prev', 'agent-swap.json', 'bin'];
  for (const name of code) {
    if (name.endsWith('.json')) fs.writeFileSync(path.join(base, name), '{}\n');
    else fs.mkdirSync(path.join(base, name, 'inside'), { recursive: true });
  }
  fs.writeFileSync(path.join(base, 'config.json'), '{"sentinel":"leave exactly alone"}\n');
  let holder = null;
  if (process.platform === 'win32') holder = spawn(process.execPath, ['-e', 'setInterval(() => {}, 1000)'], { cwd: bin, stdio: 'ignore' });
  else fs.chmodSync(bin, 0o500);
  t.after(async () => {
    if (holder) await new Promise((resolve) => { holder.on('exit', resolve); holder.kill(); });
    else fs.chmodSync(bin, 0o700);
    fs.rmSync(root, { recursive: true, force: true });
  });

  const held = runRemoval(base, 'purge');
  assert.equal(held.status, 1, held.stdout);
  assert.match(held.stderr, /bin.*; nothing was removed\n$/);
  assert.equal(held.stdout, '');
  assert.deepEqual(fs.readdirSync(base).sort(), [...code, 'config.json', 'op.lock.sqlite'].sort());
  for (const name of code.filter((each) => !each.endsWith('.json'))) assert.equal(fs.existsSync(path.join(base, name, 'inside')), true, name);
  assert.equal(fs.readFileSync(path.join(base, 'config.json'), 'utf8'), '{"sentinel":"leave exactly alone"}\n');
});

// A file in a folder that may not be written to cannot be deleted. Windows has no such folder for
// its owner, so this runs elsewhere.
test('what cannot be deleted once it is aside is named, and the next uninstall takes it', { skip: process.platform === 'win32' || process.getuid?.() === 0 }, (t) => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'legion removal left '));
  const base = path.join(root, '.legion-control');
  const locked = path.join(base, 'agent', 'locked');
  fs.mkdirSync(locked, { recursive: true });
  fs.writeFileSync(path.join(locked, 'index.mjs'), '\n');
  fs.mkdirSync(path.join(base, 'bin'));
  fs.chmodSync(locked, 0o500);
  t.after(() => {
    const folders = fs.existsSync(base) ? fs.readdirSync(base, { recursive: true }).map((name) => path.join(base, name)) : [];
    for (const folder of folders) if (fs.lstatSync(folder).isDirectory()) fs.chmodSync(folder, 0o700);
    fs.rmSync(root, { recursive: true, force: true });
  });

  const first = runRemoval(base, 'code');
  assert.equal(first.status, 0, first.stderr);
  const lines = first.stdout.trim().split('\n');
  assert.deepEqual(lines.slice(0, 2), [`removed ${path.join(base, 'agent')}`, `removed ${path.join(base, 'bin')}`]);
  assert.match(lines[2], /^could not delete all of .*agent\.removed\.[^ ]+ yet, and the next uninstall does: \S/);
  assert.equal(lines.length, 3);
  const aside = fs.readdirSync(base).filter((name) => name.startsWith('agent.removed.'));
  assert.equal(aside.length, 1, fs.readdirSync(base).join(', '));
  assert.equal(fs.existsSync(path.join(base, 'agent')), false);

  fs.chmodSync(path.join(base, aside[0], 'agent', 'locked'), 0o700);
  const second = runRemoval(base, 'purge');
  assert.equal(second.status, 0, second.stderr);
  assert.equal(second.stdout, `removed ${path.join(base, aside[0])}\nremoved ${base}\n`);
  assert.equal(fs.existsSync(base), false);
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

// Windows PowerShell, the way an SSH session and the documented command run the installer.
function runScript(source, base, home, args = []) {
  const script = path.join(source, 'install', 'install-windows.ps1');
  return spawnSync('powershell.exe', ['-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-File', script, ...args], {
    encoding: 'utf8',
    timeout: 180000,
    env: { ...process.env, PATH: `${path.dirname(process.execPath)}${path.delimiter}${process.env.PATH}`, LEGIONCTL_HOME: base, USERPROFILE: home },
  });
}

// With the scheduler left alone: the test runs as whoever the runner is and must not leave a task
// behind.
const runInstaller = (source, base, home, args = []) => runScript(source, base, home, ['-SkipScheduler', ...args]);

// What Windows PowerShell answers to `script`, read as JSON. Values reach it through the
// environment, so nothing has to be quoted for it.
function powershellJson(script, env) {
  const result = spawnSync('powershell.exe', ['-NoProfile', '-NonInteractive', '-Command', script], { encoding: 'utf8', timeout: 60000, env: { ...process.env, ...env } });
  assert.equal(result.status, 0, `${result.stdout}\n${result.stderr}`);
  return JSON.parse(result.stdout);
}

// Who the ACL of `directory` lets in, by SID, whether it inherits anything, who owns it, and the
// SID of the account the test runs as.
const accessOf = (directory) => powershellJson(
  '$acl = Get-Acl -LiteralPath $env:LEGION_TEST_PATH; $sid = [Security.Principal.SecurityIdentifier]; ' +
  '[pscustomobject] @{ protected = $acl.AreAccessRulesProtected; owner = $acl.GetOwner($sid).Value; ' +
  'holders = @($acl.GetAccessRules($true, $true, $sid) | ForEach-Object { $_.IdentityReference.Value } | Sort-Object -Unique); ' +
  'user = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value } | ConvertTo-Json -Compress',
  { LEGION_TEST_PATH: directory },
);

// The scheduled task `name` as the scheduler has it: whom it runs as, by SID, its time limit, its
// action and its state.
const taskOf = (name) => powershellJson(
  '$task = Get-ScheduledTask -TaskName $env:LEGION_TEST_TASK; ' +
  '$account = New-Object Security.Principal.NTAccount($task.Principal.UserId); ' +
  '[pscustomobject] @{ sid = $account.Translate([Security.Principal.SecurityIdentifier]).Value; limit = $task.Settings.ExecutionTimeLimit; ' +
  'execute = $task.Actions[0].Execute; arguments = $task.Actions[0].Arguments; state = [string] $task.State } | ConvertTo-Json -Compress',
  { LEGION_TEST_TASK: name },
);

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
  // The base, and so everything in it, is the installing account's alone.
  const access = accessOf(base);
  assert.equal(access.protected, true);
  assert.deepEqual(access.holders, [access.user]);
  assert.match(first.stdout, /sshd DefaultShell: /);
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

test('the Windows uninstaller removes the launchers and the agent, and keeps its config and records until -Purge', { skip: process.platform !== 'win32' }, (t) => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'legion windows uninstall '));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const home = path.join(root, "home with ' apostrophe");
  const base = path.join(home, '.legion-control');
  const source = agentSource(root);
  // A task name nothing has, so uninstalling finds no task and touches the scheduler not at all.
  const name = taskName();
  const installed = runInstaller(source, base, home, ['-TaskName', name]);
  assert.equal(installed.status, 0, `${installed.stdout}\n${installed.stderr}`);
  fs.writeFileSync(path.join(base, 'config.json'), '{"sentinel":"leave exactly alone"}\n');
  fs.mkdirSync(path.join(base, 'ops'), { recursive: true });
  fs.writeFileSync(path.join(base, 'ops', 'op-1.json'), '{}\n');
  fs.mkdirSync(path.join(base, 'private'));
  fs.writeFileSync(path.join(base, 'private', 'terms.txt'), 'terms\n');

  const removed = runScript(source, base, home, ['-Uninstall', '-TaskName', name]);
  assert.equal(removed.status, 0, `${removed.stdout}\n${removed.stderr}`);
  assert.match(removed.stdout, /no scheduled task '.*' to remove/);
  for (const gone of ['bin', 'agent']) assert.equal(fs.existsSync(path.join(base, gone)), false, gone);
  assert.equal(fs.readFileSync(path.join(base, 'config.json'), 'utf8'), '{"sentinel":"leave exactly alone"}\n');
  assert.equal(fs.existsSync(path.join(base, 'ops', 'op-1.json')), true);
  assert.match(removed.stdout, /-Uninstall -Purge/);

  const purged = runScript(source, base, home, ['-Uninstall', '-Purge', '-TaskName', name]);
  assert.equal(purged.status, 0, `${purged.stdout}\n${purged.stderr}`);
  assert.deepEqual(fs.readdirSync(base), ['private']);
  assert.match(purged.stdout, /kept .*private, which is not the agent's/);
  assert.equal(taskExists(name), false);
});

// Given to one account alone, a profile would lose every other account's access, SYSTEM's too, and
// uninstalling from it would take its bin folder.
test('the Windows installer refuses a base that is a profile, installing or uninstalling', { skip: process.platform !== 'win32' }, (t) => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'legion windows profile base '));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const home = path.join(root, 'home');
  fs.mkdirSync(path.join(home, 'bin'), { recursive: true });
  fs.writeFileSync(path.join(home, 'bin', 'tool.cmd'), '@echo mine\r\n');
  const before = accessOf(home);
  const source = agentSource(root);
  for (const args of [['-SkipScheduler'], ['-Uninstall'], ['-Uninstall', '-Purge']]) {
    const refused = runScript(source, home, home, ['-TaskName', taskName(), ...args]);
    assert.equal(refused.status, 1, `${args.join(' ')}\n${refused.stdout}\n${refused.stderr}`);
    assert.match(`${refused.stdout}\n${refused.stderr}`, /LEGIONCTL_HOME is .*, which is not a folder of the agent's own\. .* Nothing was changed\./);
  }
  assert.deepEqual(accessOf(home), before);
  assert.deepEqual(fs.readdirSync(home), ['bin']);
  assert.equal(fs.readFileSync(path.join(home, 'bin', 'tool.cmd'), 'utf8'), '@echo mine\r\n');
});

// --- The scheduler, on CI's Windows only ---------------------------------------------

// These register real tasks, one of them as SYSTEM, and so run only on a throwaway runner, which is
// elevated. Each task has a name of its own and is deleted again whatever happens.
const windowsCi = process.platform === 'win32' && process.env.GITHUB_ACTIONS === 'true';
const sameFile = (a, b) => path.resolve(a).toLowerCase() === path.resolve(b).toLowerCase();

test('the Windows installer registers the task as the installing user with the cycle budget, and -Uninstall takes it away', { skip: !windowsCi }, (t) => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'legion windows task '));
  const name = taskName();
  t.after(() => {
    spawnSync('schtasks.exe', ['/Delete', '/TN', name, '/F']);
    fs.rmSync(root, { recursive: true, force: true });
  });
  const home = path.join(root, "home with ' apostrophe");
  const base = path.join(home, '.legion-control');
  const source = agentSource(root);

  const created = runScript(source, base, home, ['-TaskName', name]);
  assert.equal(created.status, 0, `${created.stdout}\n${created.stderr}`);
  assert.match(created.stdout, /does not exist yet/);
  let task = taskOf(name);
  const access = accessOf(base);
  assert.equal(task.sid, access.user);
  assert.equal(task.limit, 'PT30M');
  assert.ok(sameFile(task.execute, process.execPath), task.execute);
  assert.equal(task.arguments, `"${path.join(base, 'bin', 'launcher.mjs')}" cycle`);
  assert.deepEqual(access.holders, [access.user]);
  // So that no cycle starts while the test runs. A disabled task stays disabled when rewritten.
  assert.equal(spawnSync('schtasks.exe', ['/Change', '/TN', name, '/DISABLE'], { encoding: 'utf8' }).status, 0);

  // Run again, the task is rewritten in place, and the XML it was rewritten from is gone.
  const rewritten = runScript(source, base, home, ['-TaskName', name]);
  assert.equal(rewritten.status, 0, `${rewritten.stdout}\n${rewritten.stderr}`);
  assert.match(rewritten.stdout, /runs as .*, the account installing it/);
  assert.match(rewritten.stdout, /time limit: +already PT30M/);
  assert.match(rewritten.stdout, /re-registered/);
  assert.deepEqual(fs.readdirSync(base).filter((entry) => entry.toLowerCase().endsWith('.xml')), []);
  task = taskOf(name);
  assert.equal(task.sid, access.user);
  assert.equal(task.limit, 'PT30M');

  const removed = runScript(source, base, home, ['-Uninstall', '-TaskName', name]);
  assert.equal(removed.status, 0, `${removed.stdout}\n${removed.stderr}`);
  assert.equal(taskExists(name), false);
  for (const gone of ['bin', 'agent']) assert.equal(fs.existsSync(path.join(base, gone)), false, gone);
  const purged = runScript(source, base, home, ['-Uninstall', '-Purge', '-TaskName', name]);
  assert.equal(purged.status, 0, `${purged.stdout}\n${purged.stderr}`);
  assert.equal(fs.existsSync(base), false);
});

test('the Windows installer refuses a task that runs as SYSTEM, and keeps one only on -AllowSystemPrincipal\'s terms', { skip: !windowsCi }, (t) => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'legion windows system '));
  const name = taskName();
  const machineBase = path.join(process.env.ProgramData ?? 'C:\\ProgramData', `legion-control-test-${process.pid}-${Date.now()}`);
  t.after(() => {
    spawnSync('schtasks.exe', ['/Delete', '/TN', name, '/F']);
    fs.rmSync(root, { recursive: true, force: true });
    fs.rmSync(machineBase, { recursive: true, force: true });
  });
  const home = path.join(root, 'home');
  const base = path.join(home, '.legion-control');
  const source = agentSource(root);
  // A task that runs as SYSTEM, first due a year from now.
  powershellJson(
    "Register-ScheduledTask -TaskName $env:LEGION_TEST_TASK -Action (New-ScheduledTaskAction -Execute 'cmd.exe' -Argument '/c exit 0') " +
    "-Trigger (New-ScheduledTaskTrigger -Once -At (Get-Date).AddYears(1)) " +
    "-Principal (New-ScheduledTaskPrincipal -UserId 'NT AUTHORITY\\SYSTEM' -LogonType ServiceAccount -RunLevel Highest) | Out-Null; '{}'",
    { LEGION_TEST_TASK: name },
  );

  // Refused, before anything was installed, with -SkipScheduler as well.
  for (const args of [[], ['-SkipScheduler']]) {
    const refused = runScript(source, base, home, ['-TaskName', name, ...args]);
    assert.equal(refused.status, 1, `${refused.stdout}\n${refused.stderr}`);
    assert.match(`${refused.stdout}\n${refused.stderr}`, /runs as SYSTEM, and whoever can write the agent's files would run code as SYSTEM through it\. Nothing was changed\./);
    assert.equal(fs.existsSync(base), false);
  }

  // Kept only from a base outside every profile.
  const inProfile = runScript(source, base, home, ['-TaskName', name, '-AllowSystemPrincipal']);
  assert.equal(inProfile.status, 1, `${inProfile.stdout}\n${inProfile.stderr}`);
  assert.match(`${inProfile.stdout}\n${inProfile.stderr}`, /is inside a user profile, and that user could change what SYSTEM runs/);
  assert.equal(fs.existsSync(base), false);

  const kept = runScript(source, machineBase, home, ['-TaskName', name, '-AllowSystemPrincipal']);
  assert.equal(kept.status, 0, `${kept.stdout}\n${kept.stderr}`);
  assert.match(kept.stdout, /runs as SYSTEM, kept by -AllowSystemPrincipal/);
  const task = taskOf(name);
  assert.equal(task.sid, 'S-1-5-18');
  assert.equal(task.limit, 'PT30M');
  assert.equal(task.arguments, `"${path.join(machineBase, 'bin', 'launcher.mjs')}" cycle`);
  const access = accessOf(machineBase);
  assert.equal(access.protected, true);
  assert.deepEqual(access.holders, ['S-1-5-18', 'S-1-5-32-544']);
  assert.equal(access.owner, 'S-1-5-32-544');
  assert.equal(accessOf(path.join(machineBase, 'bin', 'launcher.mjs')).owner, 'S-1-5-32-544');

  const removed = runScript(source, machineBase, home, ['-Uninstall', '-Purge', '-TaskName', name]);
  assert.equal(removed.status, 0, `${removed.stdout}\n${removed.stderr}`);
  assert.equal(taskExists(name), false);
  assert.equal(fs.existsSync(machineBase), false);
});
