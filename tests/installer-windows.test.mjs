import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import test from 'node:test';
import { checkoutFiles } from '../scripts/checkout-files.mjs';

const repository = path.resolve(import.meta.dirname, '..');
const installer = fs.readFileSync(path.join(repository, 'agent', 'install', 'install-windows.ps1'), 'utf8');
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

// --- The scheduler, on CI's Windows only ---------------------------------------------

// These register real tasks, one of them as SYSTEM, and so run only on a throwaway runner, which is
// elevated. Each task has a name of its own and is deleted again whatever happens.
const windowsCi = process.platform === 'win32' && process.env.GITHUB_ACTIONS === 'true';
const sameFile = (a, b) => path.resolve(a).toLowerCase() === path.resolve(b).toLowerCase();

test('the Windows installer registers the task as the installing user with the cycle budget', { skip: !windowsCi }, (t) => {
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
});
