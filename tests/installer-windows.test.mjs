import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';

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
