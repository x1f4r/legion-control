#!/usr/bin/env node
// Run legion end to end between two nodes on this machine, the second one reached over real ssh.
//
//   node scripts/legion-e2e.mjs --bin <built legion> --work <directory> --far <ssh alias>
//       --gate <ssh alias> --far-home <directory> --far-bin <path> [--skip <steps>]
//
// The first node ("near") is installed from --bin with the platform's install script into --work
// and talks to the second ("far") only through `ssh <alias>`, so every routed call opens a fresh
// login session on the far side and goes through the fixed remote command shapes, the far side's
// login shell and, for files, `legion stream`. The far node is the same account reached through
// sshd with LEGION_HOME set to --far-home; --gate is a second key whose authorized_keys line forces
// `legion gate --as e2e:gate` with the binary at --far-bin, where fleet.install puts it. The
// workflow .github/workflows/legion-e2e.yml sets all of that up and explains the choices per OS.
//
// Each step prints what it checks and the run stops at the first failure, naming the step. What
// is worth keeping when it fails (both audit logs, the doctor's answers, every call this script
// made and what came back) goes to <work>/artifacts; nothing in it is key material.
import { spawn, spawnSync } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { isDeepStrictEqual } from 'node:util';

const repository = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const windows = process.platform === 'win32';
const expectedOs = { linux: 'linux', darwin: 'macos', win32: 'windows' }[process.platform];

// Every argument that crosses a hop, as data: spaces, both quotes, what sh, cmd and PowerShell
// expand or split on, backslashes (also at the end, where Windows quoting is easiest to get
// wrong), non-ASCII text in three scripts and an emoji outside the Basic Multilingual Plane.
const TRICKY = [
  'plain',
  'with space',
  ' leading and trailing ',
  '',
  'double"quote',
  "single'quote",
  '$HOME ${PATH} $(id) `id`',
  '%PATH% %% %USERPROFILE%',
  'a & b && c || d | e ; f',
  '<in >out 2>&1',
  '^caret !bang (paren) {brace} [bracket] *star ?what #hash ~tilde',
  'back\\slash \\\\server\\share',
  'trailing backslash\\',
  'quote at end\\"',
  'ünïcødé ✓ Straße 日本語 Ελληνικά 😀',
  'tab\there',
];
const TRICKY_TEXT = TRICKY.join(' | ');

function usage(message) {
  if (message) console.error(`legion-e2e: ${message}`);
  console.error('Usage: node scripts/legion-e2e.mjs --bin <legion> --work <dir> --far <alias> --gate <alias> --far-home <dir> --far-bin <path> [--skip <steps>]');
  process.exit(2);
}

function parseArgs(argv) {
  const options = {};
  for (let i = 0; i < argv.length; i += 1) {
    const key = argv[i];
    if (!key.startsWith('--') || i + 1 >= argv.length) usage(`unexpected argument ${key}`);
    options[key.slice(2)] = argv[(i += 1)];
  }
  for (const required of ['bin', 'work', 'far', 'gate', 'far-home', 'far-bin']) {
    if (!options[required]) usage(`--${required} is required`);
  }
  return {
    bin: path.resolve(options.bin),
    work: path.resolve(options.work),
    far: options.far,
    gate: options.gate,
    farHome: path.resolve(options['far-home']),
    farBin: options['far-bin'],
    // Step names or their letters (`h,i3`) to leave out, for a machine without, say, systemd.
    skip: (options.skip ?? '').split(',').map(s => s.trim()).filter(Boolean),
  };
}

const opts = parseArgs(process.argv.slice(2));
const run = crypto.randomBytes(4).toString('hex');
const farRoot = `~/legion-e2e-${run}`;
const artifacts = path.join(opts.work, 'artifacts');
const nearHome = path.join(opts.work, 'near');
const data = path.join(opts.work, 'data');
const transcript = [];
let near = null;

class Failure extends Error {}

function check(condition, message, detail) {
  if (!condition) {
    throw new Failure(detail === undefined ? message : `${message}\n${typeof detail === 'string' ? detail : JSON.stringify(detail, null, 2)}`);
  }
}

function clip(text, limit = 4000) {
  const value = String(text ?? '');
  return value.length > limit ? `${value.slice(0, limit)}... (${value.length - limit} more characters)` : value;
}

function record(entry) {
  transcript.push(entry);
}

/** The environment every near-node process gets: its own LEGION_HOME and nothing else of ours. */
function nearEnv(extra = {}) {
  const env = { ...process.env, ...extra };
  for (const name of Object.keys(env)) {
    if (name.startsWith('LEGION_') && !(name in extra)) delete env[name];
  }
  env.LEGION_HOME = nearHome;
  return env;
}

/** Runs a program to completion and returns its exit code and output, recording the call. */
function exec(program, args, { input, env, timeout = 180_000 } = {}) {
  const started = Date.now();
  const result = spawnSync(program, args, {
    input: input ?? '',
    env: env ?? process.env,
    encoding: 'utf8',
    maxBuffer: 256 * 1024 * 1024,
    timeout,
    windowsHide: true,
  });
  const entry = {
    program,
    args,
    stdin: input ? clip(input, 2000) : undefined,
    status: result.status,
    signal: result.signal,
    error: result.error ? String(result.error) : undefined,
    ms: Date.now() - started,
    stdout: clip(result.stdout),
    stderr: clip(result.stderr),
  };
  record(entry);
  if (result.error) {
    throw new Failure(`${path.basename(program)} ${args.join(' ')} could not run: ${result.error.message}`);
  }
  return { status: result.status, stdout: result.stdout, stderr: result.stderr, entry };
}

/**
 * Calls a tool through the near node's command line. Arguments travel as JSON on stdin, so no
 * value is ever on a command line. Returns `{ ok, value }` or `{ ok: false, error }`.
 */
function call(tool, args = {}, { on, as, timeout } = {}) {
  const argv = [tool, '--args', '-', '--json'];
  if (on) argv.push('--on', on);
  if (as) argv.push('--as', as);
  return parseOutcome(exec(near, argv, { input: JSON.stringify(args), env: nearEnv(), timeout }), tool);
}

function parseOutcome(result, what) {
  const lines = result.stdout.split('\n').filter(line => line.trim() !== '');
  let value;
  try {
    value = JSON.parse(lines.at(-1));
  } catch {
    throw new Failure(`${what} exited ${result.status} without a JSON answer on stdout.\nstdout: ${clip(result.stdout)}\nstderr: ${clip(result.stderr)}`);
  }
  if (result.status === 0) return { ok: true, value, stderr: result.stderr };
  if (result.status === 1) return { ok: false, error: value, stderr: result.stderr };
  throw new Failure(`${what} exited ${result.status}.\nstdout: ${clip(result.stdout)}\nstderr: ${clip(result.stderr)}`);
}

function ok(tool, args, options) {
  const outcome = call(tool, args, options);
  check(outcome.ok, `${tool}${options?.on ? ` --on ${options.on}` : ''}${options?.as ? ` --as ${options.as}` : ''} failed`, outcome.error);
  return outcome.value;
}

function refused(tool, args, options, code = 'denied') {
  const outcome = call(tool, args, options);
  check(!outcome.ok, `${tool}${options?.on ? ` --on ${options.on}` : ''}${options?.as ? ` --as ${options.as}` : ''} succeeded but must be refused`, outcome.value);
  check(outcome.error.code === code, `${tool} was refused with ${outcome.error.code}, expected ${code}`, outcome.error);
  return outcome.error;
}

/** `legion cp` with flags, as a person would type it. */
function cp(from, to, flags = []) {
  const outcome = parseOutcome(exec(near, ['cp', from, to, ...flags, '--json'], { env: nearEnv(), timeout: 600_000 }), 'legion cp');
  check(outcome.ok, `legion cp ${from} ${to} ${flags.join(' ')} failed`, outcome.error);
  return outcome.value;
}

function sha256(file) {
  return crypto.createHash('sha256').update(fs.readFileSync(file)).digest('hex');
}

/** Every entry under `root`: relative path (with /), kind, and for files size and SHA-256. */
function tree(root) {
  const entries = [];
  const walk = (dir, prefix) => {
    for (const name of fs.readdirSync(dir).sort()) {
      const full = path.join(dir, name);
      const rel = prefix ? `${prefix}/${name}` : name;
      const stat = fs.lstatSync(full);
      if (stat.isDirectory()) {
        entries.push({ path: rel, kind: 'dir' });
        walk(full, rel);
      } else {
        entries.push({ path: rel, kind: 'file', size: stat.size, sha256: sha256(full) });
      }
    }
  };
  walk(root, '');
  return entries;
}

function sameVersion(a, b) {
  return a.trim() === b.trim();
}

function sleep(ms) {
  return new Promise(resolve => setTimeout(resolve, ms));
}

/** Runs a PowerShell script without any quoting through a command line. */
function powershell(script, env) {
  const encoded = Buffer.from(script, 'utf16le').toString('base64');
  const result = exec('powershell.exe', ['-NoProfile', '-NonInteractive', '-EncodedCommand', encoded], { env });
  check(result.status === 0, 'PowerShell failed', result.stderr || result.stdout);
  return result.stdout;
}

/** The current user's PATH as the registry keeps it, unexpanded, or null when there is none. */
function userPath() {
  const out = powershell("$k = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment'); $v = $k.GetValue('Path', $null, 'DoNotExpandEnvironmentNames'); if ($null -eq $v) { 'none' } else { $k.GetValueKind('Path').ToString() + ' ' + [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes([string]$v)) }").trim();
  if (out === 'none') return null;
  const [kind, encoded] = out.split(' ');
  return { kind, value: Buffer.from(encoded ?? '', 'base64').toString('utf8') };
}

function restoreUserPath(saved) {
  const script = saved === null
    ? "$k = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment', $true); $k.DeleteValue('Path', $false)"
    : "$k = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment', $true); $k.SetValue('Path', $env:E2E_PATH_VALUE, [Enum]::Parse([Microsoft.Win32.RegistryValueKind], $env:E2E_PATH_KIND))";
  powershell(script, { ...process.env, E2E_PATH_VALUE: saved?.value ?? '', E2E_PATH_KIND: saved?.kind ?? '' });
}

/** A command for `exec.run cmd` / extension `cmd` that writes an environment variable verbatim. */
function printEnvCommand(name) {
  return windows ? `[Console]::Out.Write($env:${name})` : `printf '%s' "$${name}"`;
}

/** A command for `exec.run cmd` that copies its standard input to its standard output. */
function catCommand() {
  return windows ? '[Console]::Out.Write([Console]::In.ReadToEnd())' : 'cat';
}

// Node prints its own argv as JSON; the far side runs the same node binary this script runs
// under, since both nodes are on this machine.
const NODE = process.execPath;
const ECHO_ARGS = 'process.stdout.write(JSON.stringify(process.argv.slice(1)))';

// A step runs unless a step it needs failed. Installing, initialising and reaching far (a and b)
// are needed by everything, so a failure there ends the run; past them, every failure is
// reported and the steps that do not depend on it still run, so one run shows every defect.
const steps = [];
function step(name, body, needs = []) {
  steps.push({ name, body, needs, fatal: /^[ab]/.test(name) });
}

// --- a. install, init, doctor -------------------------------------------------------------------

step('a1 install the built binary with the platform install script', () => {
  const expected = exec(opts.bin, ['--version']).stdout.trim();
  check(/^legion \d+\.\d+\.\d+/.test(expected), `${opts.bin} --version printed something unexpected`, expected);
  if (windows) {
    const local = path.join(opts.work, 'install-local');
    fs.mkdirSync(local, { recursive: true });
    const before = userPath();
    const script = path.join(repository, 'install', 'legion.ps1');
    const result = exec('powershell.exe', ['-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-File', script, '-From', opts.bin], { env: { ...process.env, LOCALAPPDATA: local } });
    check(result.status === 0, 'install/legion.ps1 -From failed', result.stderr || result.stdout);
    const installDir = path.join(local, 'legion');
    const after = userPath();
    check(after && after.value.split(';').some(entry => entry.toLowerCase() === installDir.toLowerCase()), `install/legion.ps1 did not add ${installDir} to the user's PATH`, after);
    // The far node is this same account, and sshd builds its sessions' environment from the
    // account's registry: left in place, this entry would make the far side find the near
    // node's binary. Restoring it keeps the two installs apart; the check above already saw the
    // installer do its part.
    restoreUserPath(before);
    near = path.join(installDir, 'legion.exe');
  } else {
    const home = path.join(opts.work, 'install-home');
    fs.mkdirSync(home, { recursive: true });
    const result = exec('sh', [path.join(repository, 'install', 'legion.sh'), '--from', opts.bin], { env: { ...process.env, HOME: home } });
    check(result.status === 0, 'install/legion.sh --from failed', result.stderr || result.stdout);
    near = path.join(home, '.local', 'bin', 'legion');
  }
  check(fs.existsSync(near), `the installer did not put legion at ${near}`);
  const installed = exec(near, ['--version']).stdout.trim();
  check(sameVersion(installed, expected), `the installed binary says ${installed}, the built one ${expected}`);
  return installed;
});

step('a2 legion init names the near node and writes its files under its own LEGION_HOME', () => {
  const outcome = parseOutcome(exec(near, ['init', '--name', 'near', '--json'], { env: nearEnv() }), 'legion init');
  check(outcome.ok, 'legion init failed', outcome.error);
  check(outcome.value.node === 'near', 'legion init did not name the node near', outcome.value);
  for (const file of ['node.toml', 'policy.toml']) {
    check(fs.existsSync(path.join(nearHome, 'config', file)), `legion init did not create ${file} under ${nearHome}`, outcome.value);
  }
});

step('a3 meta.doctor finds nothing wrong on a fresh node', () => {
  const doctor = ok('meta.doctor', {});
  fs.writeFileSync(path.join(artifacts, 'doctor-near-fresh.json'), JSON.stringify(doctor, null, 2));
  const failing = doctor.checks.filter(c => !c.ok);
  check(doctor.ok === true && failing.length === 0, 'meta.doctor reported problems', failing);
  return doctor.checks.filter(c => c.warning).map(c => `warning: ${c.name}`).join(', ') || 'no warnings';
});

// --- b. fleet.install and ping -------------------------------------------------------------------

step('b1 fleet.add the far machine by its ssh alias', () => {
  const added = ok('fleet.add', { name: 'far', ssh: opts.far });
  check(added.added === true, 'fleet.add did not add far', added);
});

step('b2 fleet.ping says far is unreachable before legion is installed there', () => {
  const ping = ok('fleet.ping', { machines: ['far'] }, { timeout: 120_000 });
  const far = ping.results.find(r => r.machine === 'far');
  check(far && far.reachable === false, 'far answered before legion was installed on it; something else answers there', ping);
  return far.error?.code;
});

let install;
step('b3 fleet.install from= copies the built binary to far and verifies it there', () => {
  install = ok('fleet.install', { machine: 'far', from: opts.bin }, { timeout: 300_000 });
  check(install.os === expectedOs, `fleet.install detected ${install.os}, this is ${expectedOs}`, install);
  check(install.dry_run !== true, 'fleet.install reported a dry run', install);
  const version = exec(opts.bin, ['--version']).stdout.trim().split(' ')[1];
  check(install.installed_version === version, `fleet.install installed ${install.installed_version}, expected ${version}`, install);
  const same = windows ? install.path.toLowerCase() === opts.farBin.toLowerCase() : install.path === opts.farBin;
  check(same, `fleet.install put legion at ${install.path}; the gate key expects ${opts.farBin}`, install);
  return `${install.os} ${install.arch} ${install.target} at ${install.path}`;
});

step('b4 fleet.ping reaches far over ssh', () => {
  const ping = ok('fleet.ping', {});
  const far = ping.results.find(r => r.machine === 'far');
  check(far && far.reachable === true, 'far is not reachable after fleet.install', ping);
  check(far.os === expectedOs, `far reports os ${far.os}`, far);
  return `${far.latency_ms} ms`;
});

step('b5 init --on far names the far node, in the LEGION_HOME sshd gives it', () => {
  const outcome = parseOutcome(exec(near, ['init', '--name', 'far', '--on', 'far', '--json'], { env: nearEnv() }), 'legion init --on far');
  check(outcome.ok, 'legion init --on far failed', outcome.error);
  check(outcome.value.node === 'far', 'init --on far did not name the node far', outcome.value);
  const nodeToml = path.join(opts.farHome, 'config', 'node.toml');
  check(fs.existsSync(nodeToml), `the far node did not write ${nodeToml}: its sessions did not get LEGION_HOME=${opts.farHome}`, outcome.value);
  check(fs.readFileSync(path.join(nearHome, 'config', 'node.toml'), 'utf8').includes('near'), 'the near node.toml changed');
});

step('b6 meta.doctor on near now pings far too', () => {
  const doctor = ok('meta.doctor', {});
  fs.writeFileSync(path.join(artifacts, 'doctor-near.json'), JSON.stringify(doctor, null, 2));
  const failing = doctor.checks.filter(c => !c.ok);
  check(doctor.ok === true && failing.length === 0, 'meta.doctor reported problems', failing);
  check(doctor.checks.some(c => /far/.test(`${c.name} ${c.detail}`)), 'meta.doctor did not check far', doctor.checks);
  const farDoctor = ok('meta.doctor', {}, { on: 'far' });
  fs.writeFileSync(path.join(artifacts, 'doctor-far.json'), JSON.stringify(farDoctor, null, 2));
  check(farDoctor.node === 'far', 'meta.doctor --on far did not run on far', farDoctor);
  const farFailing = farDoctor.checks.filter(c => !c.ok);
  check(farFailing.length === 0, 'meta.doctor --on far reported problems', farFailing);
});

// --- c. routed calls -----------------------------------------------------------------------------

step('c1 meta.version --on far answers as far', () => {
  const version = ok('meta.version', {}, { on: 'far' });
  check(version.node === 'far' && version.os === expectedOs, 'meta.version --on far answered wrongly', version);
});

step('c2 sys.info --on far', () => {
  const info = ok('sys.info', {}, { on: 'far' });
  check(info.os && typeof info.hostname === 'string' && info.hostname.length > 0, 'sys.info --on far has no host name', info);
  return `${info.hostname}, ${info.os.name ?? JSON.stringify(info.os)}`;
});

step('c3 exec.run argv --on far passes every argument through byte for byte', () => {
  const result = ok('exec.run', { argv: [NODE, '-e', ECHO_ARGS, ...TRICKY] }, { on: 'far' });
  check(result.exit_code === 0, 'the echo program failed on far', result);
  check(result.stdout === JSON.stringify(TRICKY), 'the arguments came back changed', { sent: TRICKY, received: result.stdout });
});

step('c4 exec.run cmd --on far keeps an environment value byte for byte', () => {
  const echoed = ok('exec.run', { cmd: printEnvCommand('E2E_VALUE'), env: { E2E_VALUE: TRICKY_TEXT } }, { on: 'far' });
  check(echoed.exit_code === 0, 'the shell command failed on far', echoed);
  check(echoed.stdout === TRICKY_TEXT, 'the environment value came back changed', { sent: TRICKY_TEXT, received: echoed.stdout });
});

step('c5 exec.run cmd --on far keeps stdin byte for byte', () => {
  const input = `${TRICKY_TEXT}\nsecond line\n`;
  const copied = ok('exec.run', { cmd: catCommand(), stdin: input }, { on: 'far' });
  check(copied.exit_code === 0, 'the stdin copy failed on far', copied);
  check(copied.stdout === input, 'stdin came back changed', { sent: input, received: copied.stdout });
});

step('c6 --on all fans out to near and far', () => {
  const outcome = parseOutcome(exec(near, ['meta.version', '--on', 'all', '--json'], { env: nearEnv() }), 'meta.version --on all');
  check(outcome.ok, 'meta.version --on all failed', outcome.error);
  const results = outcome.value.results;
  check(results?.near?.ok?.node === 'near' && results?.far?.ok?.node === 'far', 'fan-out did not answer from both nodes', outcome.value);
});

// --- d. files ------------------------------------------------------------------------------------

const textDir = `${farRoot}/files/dir with space/ünïcødé ✓`;
const textFile = `${textDir}/naïve & “quoted” $name %x%.txt`;
const textContent = 'line one\nzweite Zeile: Grüße ✓ 日本語\nthird line\n';

step('d1 fs.write --on far to a name with spaces and non-ASCII characters', () => {
  const written = ok('fs.write', { path: textFile, content: textContent, parents: true }, { on: 'far' });
  check(written.size === Buffer.byteLength(textContent), 'fs.write reported the wrong size', written);
});

step('d2 fs.read --on far returns the same bytes', () => {
  const read = ok('fs.read', { path: textFile }, { on: 'far' });
  check(read.content === textContent, 'fs.read returned different content', { sent: textContent, received: read.content });
}, ['d1']);

step('d3 fs.edit --on far changes one string and keeps the line endings', () => {
  ok('fs.edit', { path: textFile, old: 'zweite Zeile', new: 'second line' }, { on: 'far' });
  const read = ok('fs.read', { path: textFile }, { on: 'far' });
  const expected = textContent.replace('zweite Zeile', 'second line');
  check(read.content === expected, 'fs.edit did not leave the expected content', { expected, received: read.content });
  const listed = ok('fs.list', { path: textDir }, { on: 'far' });
  check(listed.entries.some(e => e.name === 'naïve & “quoted” $name %x%.txt'), 'fs.list does not show the file under its name', listed);
}, ['d1']);

const bigLocal = path.join(data, 'big.bin');
const bigBack = path.join(data, 'big-back.bin');
step('d4 legion cp a 20 MB file to far and back, with the same SHA-256', () => {
  fs.writeFileSync(bigLocal, crypto.randomBytes(20 * 1024 * 1024));
  const there = cp(bigLocal, `far:${farRoot}/big/big.bin`);
  check(there.files_sent === 1 && there.bytes_sent === 20 * 1024 * 1024, 'legion cp to far did not send the whole file once', there);
  const back = cp(`far:${farRoot}/big/big.bin`, bigBack);
  check(back.files_sent === 1, 'legion cp from far did not send the file', back);
  check(sha256(bigBack) === sha256(bigLocal), 'the file that came back differs from the one sent');
  return `${there.duration_ms} ms there, ${back.duration_ms} ms back`;
});

const treeLocal = path.join(data, 'tree');
const treeBack = path.join(data, 'tree-back');
const fixedTime = new Date('2026-01-02T03:04:05Z');
step('d5 legion cp a tree with nested and empty directories, then back', () => {
  const files = {
    'top.txt': 'top\n',
    'a/b/c/deep.txt': 'deep\n',
    'a/b/sibling.txt': 'sibling\n',
    'ü dir/ñ file with space.txt': 'non-ASCII names\n',
    'zero.bin': '',
    'same-size.txt': 'AAAA\n',
  };
  for (const [rel, content] of Object.entries(files)) {
    const file = path.join(treeLocal, ...rel.split('/'));
    fs.mkdirSync(path.dirname(file), { recursive: true });
    fs.writeFileSync(file, content);
    fs.utimesSync(file, fixedTime, fixedTime);
  }
  fs.mkdirSync(path.join(treeLocal, 'empty'), { recursive: true });
  fs.mkdirSync(path.join(treeLocal, 'a', 'empty too'), { recursive: true });
  const there = cp(treeLocal, `far:${farRoot}/tree`);
  check(there.files_sent === Object.keys(files).length, `legion cp sent ${there.files_sent} files, expected ${Object.keys(files).length}`, there);
  const back = cp(`far:${farRoot}/tree`, treeBack);
  check(isDeepStrictEqual(tree(treeBack), tree(treeLocal)), 'the tree that came back differs', { sent: tree(treeLocal), received: tree(treeBack) });
});

step('d6 a second legion cp of the same tree sends nothing', () => {
  const again = cp(treeLocal, `far:${farRoot}/tree`);
  check(again.files_sent === 0 && again.bytes_sent === 0, 'the second run sent files again', again);
}, ['d5']);

step('d7 legion cp --delete removes what the source no longer has', () => {
  fs.rmSync(path.join(treeLocal, 'a', 'b', 'c'), { recursive: true });
  fs.rmSync(path.join(treeLocal, 'empty'), { recursive: true });
  const synced = cp(treeLocal, `far:${farRoot}/tree`, ['--delete']);
  check(synced.deleted >= 2, 'legion cp --delete did not delete the removed file and directories', synced);
  const gone = call('fs.stat', { path: `${farRoot}/tree/a/b/c/deep.txt` }, { on: 'far' });
  check(!gone.ok && gone.error.code === 'not_found', 'a deleted file is still on far', gone);
  const goneDir = call('fs.stat', { path: `${farRoot}/tree/empty` }, { on: 'far' });
  check(!goneDir.ok && goneDir.error.code === 'not_found', 'a deleted directory is still on far', goneDir);
  check(ok('fs.stat', { path: `${farRoot}/tree/a/empty too` }, { on: 'far' }).kind === 'dir', 'an empty directory that is still in the source was deleted');
}, ['d5']);

step('d8 legion cp --checksum sends a file whose size and time did not change', () => {
  const file = path.join(treeLocal, 'same-size.txt');
  fs.writeFileSync(file, 'BBBB\n');
  fs.utimesSync(file, fixedTime, fixedTime);
  const quick = cp(treeLocal, `far:${farRoot}/tree`);
  check(quick.files_sent === 0, 'without --checksum a file with the same size and time was sent', quick);
  const thorough = cp(treeLocal, `far:${farRoot}/tree`, ['--checksum']);
  check(thorough.files_sent === 1, 'legion cp --checksum did not send the changed file', thorough);
  const read = ok('fs.read', { path: `${farRoot}/tree/same-size.txt` }, { on: 'far' });
  check(read.content === 'BBBB\n', 'far does not have the new content', read);
}, ['d5']);

// --- e. jobs -------------------------------------------------------------------------------------

let printJob;
step('e1 job.start --on far a command that prints, sleeps and prints again', () => {
  const script = "console.log('first'); setTimeout(() => console.log('second'), 5000)";
  printJob = ok('job.start', { argv: [NODE, '-e', script], name: 'e2e-print' }, { on: 'far' });
  check(printJob.id && ['starting', 'running'].includes(printJob.state), 'job.start did not start the job', printJob);
  return printJob.id;
});

step('e2 from new sessions, job.status and job.logs see it running with its first line', async () => {
  const status = ok('job.status', { id: printJob.id }, { on: 'far' });
  check(['starting', 'running'].includes(status.state), `the job is ${status.state} right after it started`, status);
  let logs;
  for (let i = 0; i < 20; i += 1) {
    logs = ok('job.logs', { id: printJob.id }, { on: 'far' });
    if (logs.content.includes('first')) break;
    await sleep(250);
  }
  check(logs.content.includes('first') && !logs.content.includes('second'), 'job.logs did not show only the first line while the job ran', logs);
}, ['e1']);

step('e3 job.wait --on far sees it finish with both lines', () => {
  const waited = ok('job.wait', { id: printJob.id, timeout_s: 60 }, { on: 'far', timeout: 120_000 });
  check(waited.state === 'succeeded' && waited.exit_code === 0 && waited.timed_out === false, 'the job did not succeed', waited);
  const logs = ok('job.logs', { id: printJob.id }, { on: 'far' });
  check(/first\r?\nsecond\r?\n$/.test(logs.content), 'job.logs does not have both lines in order', logs);
}, ['e1']);

step('e4 job.cancel --on far leaves no process of the job behind', async () => {
  const marker = `legion-e2e-cancel-${run}`;
  const job = ok('job.start', { argv: [NODE, '-e', 'setTimeout(() => {}, 600000)', marker], name: 'e2e-cancel' }, { on: 'far' });
  let procs;
  for (let i = 0; i < 40; i += 1) {
    procs = ok('sys.procs', { filter: marker, limit: 50 }, { on: 'far' });
    if (procs.procs.length > 0) break;
    await sleep(250);
  }
  check(procs.procs.length > 0, 'sys.procs --on far never showed the job running', procs);
  const cancelled = ok('job.cancel', { id: job.id }, { on: 'far', timeout: 120_000 });
  check(cancelled.state === 'cancelled', 'job.cancel did not report the job cancelled', cancelled);
  const after = ok('sys.procs', { filter: marker, limit: 50 }, { on: 'far' });
  check(after.procs.length === 0, 'processes of the cancelled job are still running', after.procs);
  const status = ok('job.status', { id: job.id }, { on: 'far' });
  check(status.state === 'cancelled', `job.status says ${status.state} after job.cancel`, status);
});

// --- f. permissions across the hop ---------------------------------------------------------------

const policy = `# Written by scripts/legion-e2e.mjs.
default = "owner"

[roles.observer]
allow = ["effect:read"]
deny = ["audit.*"]
paths = ["~/**"]

[roles.editor]
allow = ["effect:read", "effect:write"]
deny = ["audit.*"]
paths = ["${farRoot}/projects/**"]

[principals."e2e:observer"]
role = "observer"

[principals."e2e:editor"]
role = "editor"

[principals."e2e:gate"]
role = "editor"
`;

step('f1 config.write the same policy on both nodes', () => {
  ok('config.write', { document: 'policy.toml', content: policy });
  ok('config.write', { document: 'policy.toml', content: policy }, { on: 'far' });
  ok('fs.write', { path: `${farRoot}/projects/readme.txt`, content: 'readable\n', parents: true }, { on: 'far' });
  const whoami = ok('meta.whoami', {}, { on: 'far', as: 'e2e:editor' });
  check(whoami.principal === 'e2e:editor' && whoami.role === 'editor' && whoami.node === 'far', 'far does not see the editor principal', whoami);
});

step('f2 what each role may do across the hop succeeds', () => {
  const read = ok('fs.read', { path: `${farRoot}/projects/readme.txt` }, { on: 'far', as: 'e2e:observer' });
  check(read.content === 'readable\n', 'observer read the wrong content', read);
  ok('fs.write', { path: `${farRoot}/projects/edited.txt`, content: 'by the editor\n' }, { on: 'far', as: 'e2e:editor' });
}, ['f1']);

step('f3 what each role may not do across the hop is refused', () => {
  const write = refused('fs.write', { path: `${farRoot}/projects/nope.txt`, content: 'x' }, { on: 'far', as: 'e2e:observer' });
  const outside = refused('fs.write', { path: `${farRoot}/outside.txt`, content: 'x' }, { on: 'far', as: 'e2e:editor' });
  check(outside.machine === 'far', 'the write outside paths was not refused by far itself', outside);
  const secret = refused('fs.read', { path: '~/.ssh/config' }, { on: 'far', as: 'e2e:observer' });
  check(secret.machine === 'far' && /secret/i.test(secret.message), 'the secret location was not refused as one by far', secret);
  refused('exec.run', { argv: [NODE, '-e', '0'] }, { on: 'far', as: 'e2e:editor' });
  refused('meta.version', {}, { on: 'far', as: 'e2e:stranger' });
  return `observer write: ${write.machine}; outside paths, secret: far`;
}, ['f1']);

step('f4 both audit logs show the forwarded calls under the right principal', () => {
  const nearAudit = ok('audit.tail', { principal: 'e2e:editor', tool: 'fs.write', limit: 50 });
  const farAudit = ok('audit.tail', { principal: 'e2e:editor', tool: 'fs.write', limit: 50 }, { on: 'far' });
  for (const [where, audit] of [['near', nearAudit], ['far', farAudit]]) {
    const outcomes = audit.entries.map(e => e.outcome);
    check(outcomes.includes('ok') && outcomes.includes('denied'), `${where}'s audit log lacks the editor's allowed and refused writes`, audit.entries);
  }
  check(nearAudit.entries.every(e => e.target === 'far'), "near's audit does not name far as the target", nearAudit.entries);
  check(farAudit.entries.every(e => e.door === 'rpc'), "far's audit does not show the calls arriving over rpc", farAudit.entries);
  const secret = ok('audit.tail', { principal: 'e2e:observer', tool: 'fs.read', outcome: 'denied' }, { on: 'far' });
  check(secret.entries.length > 0, "far's audit log lacks the observer's refused read", secret);
}, ['f2', 'f3']);

// --- g. restricted key ---------------------------------------------------------------------------

step('g1 a key forced to legion gate is pinned to its principal', () => {
  ok('fleet.add', { name: 'fargate', ssh: opts.gate });
  const whoami = ok('meta.whoami', {}, { on: 'fargate' });
  check(whoami.principal === 'e2e:gate' && whoami.role === 'editor', 'the gated session is not pinned to e2e:gate', whoami);
  const claimed = ok('meta.whoami', {}, { on: 'fargate', as: 'owner' });
  check(claimed.principal === 'e2e:gate', 'asking for owner through the gated key was not ignored', claimed);
}, ['f1']);

step('g2 through the gated key, what the role allows works, files included', () => {
  ok('fs.write', { path: `${farRoot}/projects/through-gate.txt`, content: 'gated\n' }, { on: 'fargate' });
  const small = path.join(data, 'gate-small.txt');
  fs.writeFileSync(small, 'sent through the gate ✓\n');
  cp(small, `fargate:${farRoot}/projects/gate-small.txt`);
  const back = path.join(data, 'gate-small-back.txt');
  cp(`fargate:${farRoot}/projects/gate-small.txt`, back);
  check(sha256(back) === sha256(small), 'the file sent through the gate came back changed');
}, ['g1']);

step('g3 through the gated key, everything else is refused', () => {
  refused('fs.write', { path: `${farRoot}/outside-gate.txt`, content: 'x' }, { on: 'fargate' });
  refused('exec.run', { argv: [NODE, '-e', '0'] }, { on: 'fargate' });
  refused('fs.read', { path: '~/.ssh/config' }, { on: 'fargate' });
  const audit = ok('audit.tail', { principal: 'e2e:gate', limit: 50 }, { on: 'far' });
  const outcomes = new Set(audit.entries.map(e => e.outcome));
  check(outcomes.has('ok') && outcomes.has('denied'), "far's audit log does not show the gated calls", audit.entries);
}, ['g1']);

step('g4 the gated key refuses to run an arbitrary command', () => {
  for (const command of ['whoami', 'legion rpc --as owner', `${windows ? 'cmd /c echo' : 'echo'} pwned`]) {
    const result = exec('ssh', ['-T', '-o', 'BatchMode=yes', opts.gate, command], { timeout: 60_000 });
    check(result.status !== 0, `ssh ${opts.gate} ${command} succeeded`, result.entry);
    check(result.stderr.includes('legion gate:') && !result.stdout.includes('pwned'), `ssh ${opts.gate} ${command} was not refused by legion gate`, result.entry);
  }
}, ['f1']);

// --- h. services ---------------------------------------------------------------------------------

const backends = { linux: 'systemd', macos: 'launchd', windows: 'windows' };
const knownService = { linux: 'systemd-journald.service', macos: 'com.apple.mDNSResponder', windows: 'sshd' }[expectedOs];

step('h1 svc.list --on far lists the platform\'s services', () => {
  const list = ok('svc.list', { scope: 'system', limit: 200 }, { on: 'far' });
  check(list.backend === backends[expectedOs], `svc.list used ${list.backend}`, list);
  check(list.services.length > 0, 'svc.list returned no services', list);
  return `${list.services.length} services from ${list.backend}`;
});

step(`h2 svc.status --on far of ${knownService}`, () => {
  const status = ok('svc.status', { name: knownService, scope: 'system' }, { on: 'far' });
  check(status.backend === backends[expectedOs] && status.state === 'running', `${knownService} is not reported running`, status);
  const unscoped = ok('svc.status', { name: knownService }, { on: 'far' });
  check(unscoped.scope === 'system', 'svc.status without a scope did not find the system service', unscoped);
});

// --- i. power, read-only and dry runs ------------------------------------------------------------

step('i1 power.status --on far', () => {
  const status = ok('power.status', {}, { on: 'far' });
  check(typeof status.boot_id === 'string' && status.boot_id.length > 0, 'power.status --on far has no boot id', status);
  return `busy: ${status.busy?.state ?? JSON.stringify(status.busy)}`;
});

step('i2 power.restart --on far dry_run=true returns the platform\'s command and does nothing', () => {
  const plan = ok('power.restart', { dry_run: true, force: true }, { on: 'far' });
  check(plan.dry_run === true && plan.scheduled === false, 'power.restart did not stay a dry run', plan);
  const flat = plan.commands.map(argv => argv.join(' '));
  const expected = { linux: /systemctl( -i)? reboot/, macos: /shutdown -r/, windows: /shutdown \/r/ }[expectedOs];
  check(flat.some(command => expected.test(command)), `power.restart did not plan ${expected}`, plan);
  return flat.join('; ');
});

// --- j. MCP --------------------------------------------------------------------------------------

/** Talks to `legion mcp --compact` with raw JSON lines and returns each answer by id. */
async function mcpSession(messages) {
  const child = spawn(near, ['mcp', '--compact'], { env: nearEnv(), stdio: ['pipe', 'pipe', 'pipe'], windowsHide: true });
  const answers = new Map();
  let buffer = '';
  let stderr = '';
  const waiting = new Map();
  child.stderr.setEncoding('utf8');
  child.stderr.on('data', chunk => { stderr += chunk; });
  child.stdout.setEncoding('utf8');
  child.stdout.on('data', chunk => {
    buffer += chunk;
    let index;
    while ((index = buffer.indexOf('\n')) >= 0) {
      const line = buffer.slice(0, index).trim();
      buffer = buffer.slice(index + 1);
      if (!line) continue;
      const message = JSON.parse(line);
      if (message.id !== undefined) {
        answers.set(message.id, message);
        waiting.get(message.id)?.();
      }
    }
  });
  const exited = new Promise(resolve => child.on('exit', code => resolve(code)));
  try {
    for (const message of messages) {
      child.stdin.write(`${JSON.stringify(message)}\n`);
      if (message.id === undefined) continue;
      if (!answers.has(message.id)) {
        await Promise.race([
          new Promise(resolve => waiting.set(message.id, resolve)),
          sleep(120_000).then(() => { throw new Failure(`legion mcp did not answer ${message.method} in time.\nstderr: ${clip(stderr)}`); }),
          exited.then(code => { throw new Failure(`legion mcp exited ${code} before answering ${message.method}.\nstderr: ${clip(stderr)}`); }),
        ]);
      }
    }
  } finally {
    child.stdin.end();
    await Promise.race([exited, sleep(10_000)]);
    if (child.exitCode === null) child.kill();
    record({ program: near, args: ['mcp', '--compact'], stdin: messages.map(m => JSON.stringify(m)).join('\n'), stdout: clip(JSON.stringify([...answers.values()])), stderr: clip(stderr) });
  }
  return answers;
}

step('j1 legion mcp --compact runs sys.info on far through legion_read', async () => {
  const before = ok('audit.tail', { tool: 'sys.info', limit: 1000 }, { on: 'far' }).entries.length;
  const answers = await mcpSession([
    { jsonrpc: '2.0', id: 1, method: 'initialize', params: { protocolVersion: '2025-06-18', capabilities: {}, clientInfo: { name: 'legion-e2e', version: '1' } } },
    { jsonrpc: '2.0', method: 'notifications/initialized' },
    { jsonrpc: '2.0', id: 2, method: 'tools/list' },
    { jsonrpc: '2.0', id: 3, method: 'tools/call', params: { name: 'legion_read', arguments: { tool: 'sys.info', on: 'far' } } },
    { jsonrpc: '2.0', id: 4, method: 'tools/call', params: { name: 'legion_read', arguments: { tool: 'meta.version', on: 'far' } } },
  ]);
  const names = answers.get(2)?.result?.tools?.map(t => t.name) ?? [];
  check(names.includes('legion_read'), 'tools/list does not offer legion_read', answers.get(2));
  for (const id of [3, 4]) {
    const result = answers.get(id)?.result;
    check(result && !result.isError, `tools/call ${id} failed`, answers.get(id));
  }
  const info = JSON.parse(answers.get(3).result.content[0].text);
  check(info.hostname, 'legion_read sys.info returned no host name', info);
  const version = JSON.parse(answers.get(4).result.content[0].text);
  check(version.node === 'far', 'legion_read meta.version on far did not answer as far', version);
  const after = ok('audit.tail', { tool: 'sys.info', limit: 1000 }, { on: 'far' }).entries;
  check(after.length > before, "far's audit log does not show the sys.info call from MCP", after.slice(-3));
  const nearAudit = ok('audit.tail', { tool: 'sys.info', limit: 5 }).entries;
  check(nearAudit.some(e => e.door === 'mcp' && e.target === 'far'), "near's audit log does not show the MCP call forwarded to far", nearAudit);
});

// --- k. extension tools --------------------------------------------------------------------------

step('k1 an extension tool on far gets a metacharacter-laden argument as data', () => {
  const script = "let s=''; process.stdin.setEncoding('utf8'); process.stdin.on('data', d => { s += d; }); process.stdin.on('end', () => process.stdout.write(JSON.stringify({ env: process.env.LEGION_ARG_TEXT, stdin: JSON.parse(s), tool: process.env.LEGION_TOOL, node: process.env.LEGION_NODE })))";
  ok('ext.define', {
    name: 'e2e.echo',
    description: 'Echo the text argument back as the environment and stdin carried it.',
    effect: 'read',
    run: [NODE, '-e', script],
    params: { text: { type: 'string', description: 'Any text.' } },
  }, { on: 'far' });
  const echoed = ok('e2e.echo', { text: TRICKY_TEXT }, { on: 'far' });
  check(echoed.env === TRICKY_TEXT, 'LEGION_ARG_TEXT arrived changed', { sent: TRICKY_TEXT, received: echoed.env });
  check(echoed.stdin?.text === TRICKY_TEXT, 'the JSON on stdin arrived changed', echoed);
  check(echoed.tool === 'e2e.echo' && echoed.node === 'far', 'the extension did not run as e2e.echo on far', echoed);
});

step('k2 an extension tool with a shell command on far prints the argument verbatim', () => {
  ok('ext.define', {
    name: 'e2e.shell',
    description: 'Print the text argument through the platform shell.',
    effect: 'read',
    cmd: printEnvCommand('LEGION_ARG_TEXT'),
    params: { text: { type: 'string', description: 'Any text.' } },
  }, { on: 'far' });
  const printed = ok('e2e.shell', { text: TRICKY_TEXT }, { on: 'far', as: 'e2e:observer' });
  check(printed.output === TRICKY_TEXT, 'the shell printed something else', { sent: TRICKY_TEXT, received: printed.output });
});

// --- run -----------------------------------------------------------------------------------------

function collect() {
  fs.mkdirSync(artifacts, { recursive: true });
  for (const [name, home] of [['near', nearHome], ['far', opts.farHome]]) {
    const audit = path.join(home, 'state', 'audit.jsonl');
    if (fs.existsSync(audit)) fs.copyFileSync(audit, path.join(artifacts, `audit-${name}.jsonl`));
  }
  fs.writeFileSync(path.join(artifacts, 'calls.json'), JSON.stringify(transcript, null, 2));
}

async function main() {
  fs.rmSync(nearHome, { recursive: true, force: true });
  fs.rmSync(data, { recursive: true, force: true });
  fs.mkdirSync(artifacts, { recursive: true });
  fs.mkdirSync(data, { recursive: true });
  console.log(`legion e2e: ${expectedOs}, far is ssh ${opts.far}, gated key ssh ${opts.gate}, files under ${farRoot}`);
  const failed = [];
  const skipped = [];
  for (const { name, body, needs, fatal } of steps) {
    if (opts.skip.some(prefix => name.startsWith(prefix))) {
      skipped.push(name);
      process.stdout.write(`--- ${name}: skipped as asked\n`);
      continue;
    }
    const blocker = failed.find(f => needs.some(prefix => f.name.startsWith(prefix)));
    if (blocker) {
      skipped.push(name);
      process.stdout.write(`--- ${name}: skipped, it needs "${blocker.name}"\n`);
      continue;
    }
    const started = Date.now();
    process.stdout.write(`==> ${name}\n`);
    try {
      const note = await body();
      process.stdout.write(`    ok (${Date.now() - started} ms)${note ? `: ${note}` : ''}\n`);
    } catch (error) {
      const message = error instanceof Failure ? error.message : error.stack;
      failed.push({ name, message });
      process.stdout.write(`    FAILED (${Date.now() - started} ms):\n${message.replace(/^/gm, '      ')}\n`);
      if (fatal) break;
    }
  }
  collect();
  if (failed.length > 0) {
    console.error(`\nlegion e2e FAILED on ${os.type()} at ${failed.length} step(s):`);
    for (const { name, message } of failed) console.error(`  - ${name}: ${message.split('\n')[0]}`);
    if (skipped.length > 0) console.error(`${skipped.length} step(s) did not run.`);
    console.error(`Every call and its output is in ${path.join(artifacts, 'calls.json')}.`);
    process.exit(1);
  }
  console.log(`\nlegion e2e passed: ${steps.length - skipped.length} steps${skipped.length ? `, ${skipped.length} skipped as asked` : ''}.`);
}

await main();
