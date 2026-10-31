// Conservative probes for explicitly enabled product profiles. Process rows
// stay in this process; replies never include command lines or credentials.
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawnSync } from 'node:child_process';
import { resolveClaudeUpdatePolicy, claudeTargetAllowed, compareProfileVersions, nativeClaudeVersionAt } from './service-profile-claude.mjs';
import { DESKTOP_APPLICATIONS, desktopLocations, readDesktopMetadata } from './service-profile-desktop.mjs';

// Each name belongs to one profile. Outside Windows a name is matched with its
// case, so the claude CLI is not the Claude app and the codex CLI is not the
// Codex app. Windows matches file names without case, and claude.exe is both
// there; the applications are told apart by where they are installed.
const NAMES = {
  'claude-code': ['claude', 'claude-code'],
  'claude-desktop': ['Claude', 'claude-desktop'],
  'codex-cli': ['codex', 'codex-cli'],
  'codex-desktop': ['Codex', 'ChatGPT'],
  'chatgpt-desktop': ['ChatGPT'],
  opencode: ['opencode', 'opencode-cli'],
  antigravity: ['Antigravity', 'antigravity'],
};

const WINDOWS_SYSTEM_NAMES = new Set([
  'smss.exe', 'csrss.exe', 'wininit.exe', 'services.exe', 'lsaiso.exe', 'lsass.exe',
  'mpdefendercoreservice.exe', 'msmpeng.exe', 'nissrv.exe', 'securityhealthservice.exe', 'svchost.exe',
]);
const WINDOWS_KERNEL_NAMES = new Set(['secure system', 'registry', 'memory compression']);
const WINDOWS_ISOLATED_SERVICE_IDENTITIES = new Map([
  ['ngciso.exe', { ownerSid: 'S-1-5-19', parentName: 'svchost.exe' }],
  ['sppsvc.exe', { ownerSid: 'S-1-5-20', parentName: 'services.exe' }],
]);

function identifiedWindowsSystemProcess(row, rows, systemRoot) {
  if (row.command !== null && row.command !== '') return false;
  const name = typeof row.name === 'string' ? row.name.toLowerCase() : '';
  if (row.parentPid === 0 && row.sessionId === 0 &&
      ((row.pid === 0 && name === 'system idle process') || (row.pid === 4 && name === 'system'))) return true;
  if (!Number.isInteger(row.pid) || row.pid <= 4 || !Number.isInteger(row.parentPid) || row.parentPid <= 0) return false;
  // A familiar executable name alone cannot establish the identity of a
  // process whose command line is protected. Require an OS service owner too.
  if (['S-1-5-18', 'S-1-5-19', 'S-1-5-20'].includes(row.ownerSid)) {
    if (row.sessionId === 0 && row.parentPid === 4 && WINDOWS_KERNEL_NAMES.has(name)) return row.ownerSid === 'S-1-5-18';
    if (WINDOWS_SYSTEM_NAMES.has(name) && (row.sessionId === 0 || (name === 'csrss.exe' && Number.isInteger(row.sessionId) && row.sessionId > 0))) return true;
  }
  const serviceIdentity = WINDOWS_ISOLATED_SERVICE_IDENTITIES.get(name);
  if (serviceIdentity && row.sessionId === 0 && row.ownerSid === serviceIdentity.ownerSid) {
    const parent = rows.find((candidate) => candidate.pid === row.parentPid);
    if (parent?.sessionId !== 0 || parent?.name?.toLowerCase() !== serviceIdentity.parentName) return false;
    const systemParentPath = typeof systemRoot === 'string' && path.win32.isAbsolute(systemRoot) &&
      typeof parent.executablePath === 'string' && path.win32.normalize(parent.executablePath).toLowerCase() ===
      path.win32.join(systemRoot, 'System32', serviceIdentity.parentName).toLowerCase();
    // Hello has a visible service host. The protected service manager can hide
    // its path, so licensing also requires that parent's verified SYSTEM SID.
    if (name === 'ngciso.exe') return systemParentPath;
    return parent.ownerSid === 'S-1-5-18' && (parent.executablePath == null || systemParentPath);
  }
  // Hyper-V memory accounting is represented by a VM-owned pseudo-process.
  // Confirm its owner namespace and the system VM worker in the same snapshot.
  if (row.sessionId === 0 && ['vmmem', 'vmmemwsl'].includes(name) && /^S-1-5-83-(?:\d+-){4}\d+$/.test(row.ownerSid ?? '') &&
      typeof systemRoot === 'string' && path.win32.isAbsolute(systemRoot)) {
    const parent = rows.find((candidate) => candidate.pid === row.parentPid);
    return parent?.name?.toLowerCase() === 'vmwp.exe' && typeof parent.executablePath === 'string' &&
      path.win32.normalize(parent.executablePath).toLowerCase() === path.win32.join(systemRoot, 'System32', 'vmwp.exe').toLowerCase();
  }
  return false;
}


const RUNTIME = /^(?:node|nodejs|bun|electron)(?:\.exe)?$/i;
const SCRIPT_EXTENSION = /\.(?:[cm]?js|[cm]?ts)$/i;
const INLINE = new Set(['-e', '--eval', '-p', '--print']);
// task_struct's PF_KTHREAD, in the flags field of /proc/<pid>/stat.
const KERNEL_THREAD = 0x00200000;

function firstWord(text) {
  const match = /^\s*(?:"([^"]+)"|'([^']+)'|(\S+))(?:\s+|$)/.exec(text);
  return match ? { value: match[1] ?? match[2] ?? match[3], exact: match[3] === undefined, rest: text.slice(match[0].length) } : null;
}

/** The words after argv0. argv read from the kernel is exact; a command line printed by ps is not. */
function words(rest) {
  if (Array.isArray(rest)) return rest.map((value) => ({ value: String(value), exact: true }));
  const list = [];
  for (let text = rest, word; text && list.length < 64 && (word = firstWord(text)); text = word.rest) list.push(word);
  return list;
}

/**
 * The file a runtime's script is, resolved through its links: an npx server
 * runs node_modules/.bin/<name>, a link to the package's own script. ps prints
 * argv unquoted, so a script whose path has a space in it arrives as several
 * words, and the shortest run of them that names a file is the script.
 */
function locateScript(list, index, { paths, cwd, file }) {
  const first = list[index];
  const candidates = [first.value];
  for (let end = index + 1; !first.exact && end < list.length && end < index + 16 && !list[end].exact; end += 1) {
    candidates.push(list.slice(index, end + 1).map((word) => word.value).join(' '));
  }
  const absolute = (candidate) => (paths.isAbsolute(candidate) ? candidate : typeof cwd === 'string' && paths.isAbsolute(cwd) ? paths.resolve(cwd, candidate) : null);
  for (const candidate of candidates) {
    const real = absolute(candidate) && file.real(absolute(candidate));
    if (real && file.isFile(real)) return { value: candidate, path: real, resolved: true };
  }
  // node also runs a directory, by its package.json or index.js. A word that
  // is exact still says where the script was when it cannot be resolved; one
  // ps split might be only the start of it.
  const start = absolute(first.value);
  const real = start && file.real(start);
  return { value: first.value, path: real || (first.exact && start ? paths.normalize(start) : null), resolved: Boolean(real) };
}

/**
 * The script a runtime runs. An option in one word, --name=value, says nothing
 * about where the script is and is passed over. Any other option may take the
 * next word as its value, which could then not be told from the script.
 */
function scriptOf(list, context) {
  for (let index = 0; index < list.length; index += 1) {
    const { value } = list[index];
    if (INLINE.has(value)) return { inline: list.slice(index + 1).map((word) => word.value).join(' ') };
    if (/^--(?:eval|print)=/.test(value)) return { inline: value.slice(value.indexOf('=') + 1) };
    if (/^--[^=\s]+=/.test(value)) continue;
    if (value.startsWith('-')) return null;
    return locateScript(list, index, context);
  }
  return null;
}

/**
 * What one row says its process is: the names it goes by, and the absolute
 * paths that say where its program lives. For node, bun and electron that
 * program is the script they run, never the runtime itself.
 *
 * A runtime can rename itself: Claude Code's cli.js sets process.title, and
 * Linux then shows the new name as comm and writes it over argv. The script is
 * gone from the row, and the runtime's own path still says nothing about which
 * install it runs, so such a row has only its name to go by.
 */
function identify(row, { paths, file, cwdOf }) {
  const name = String(row.name ?? '');
  const command = typeof row.command === 'string' ? row.command : '';
  let uncertain = !name || !command;
  let argv0;
  let rest;
  if (Array.isArray(row.argv) && row.argv.length > 0) [argv0, ...rest] = row.argv;
  // ps cannot quote, so a space in the executable's own path reads like the
  // one before its first argument. On a Mac comm is that path in full, and
  // the command line starts with it.
  else if (name && (command === name || command.startsWith(`${name} `))) [argv0, rest] = [name, command.slice(name.length)];
  else ({ value: argv0 = '', rest = '' } = firstWord(command) ?? {});
  const names = [name, argv0];
  const located = [];
  const runtime = (identity) => RUNTIME.test(String(identity ?? '').split(/[\\/]/).at(-1));
  if ([name, argv0].some(runtime)) {
    const script = scriptOf(words(rest), { paths, file, cwd: cwdOf?.(row.pid) });
    if (!script) uncertain = true;
    else if (script.inline !== undefined) names.push(script.inline);
    else {
      names.push(script.value);
      if (script.path) { names.push(script.path); located.push(script.path); }
      if (!script.resolved && !SCRIPT_EXTENSION.test(script.value)) uncertain = true;
    }
  } else if (!runtime(row.executablePath)) {
    for (const candidate of [row.executablePath, argv0]) if (typeof candidate === 'string' && paths.isAbsolute(candidate)) located.push(candidate);
  }
  return { names, located, uncertain };
}

/**
 * The agent's process and everything it started: its other probes are no
 * product session. `descends` says whether a row really is the child its
 * parent id names. The tree counts only when this probe is in it, which is
 * what shows the agent named is the one that started it.
 */
function agentTree(rows, agentPid, ownPid, descends) {
  const byPid = new Map(rows.map((row) => [row.pid, row]));
  const children = new Map();
  for (const row of rows) if (Number.isInteger(row.parentPid) && row.parentPid !== row.pid) children.set(row.parentPid, [...(children.get(row.parentPid) ?? []), row]);
  const tree = new Set([agentPid]);
  for (const pid of tree) for (const child of children.get(pid) ?? []) if (descends(byPid.get(pid), child)) tree.add(child.pid);
  return tree.has(ownPid) ? tree : new Set();
}

function realFiles(io) {
  return {
    real: (file) => { try { return io.realpathSync(file); } catch { return null; } },
    isFile: (file) => { try { return io.statSync(file).isFile(); } catch { return false; } },
  };
}

/**
 * The verdict for one profile over one process snapshot.
 *
 * `install.owned` is where the install an update replaces lives: the npm
 * package root and the bin links it owns, or the application bundle. When it
 * is known, a process whose program has a path counts only when that path is
 * inside it, whatever the process is called; a name decides only for a
 * process whose path cannot be read. `install.foreign` is where other
 * profiles' applications live, and a name never claims a process from there.
 *
 * `agentPid` is the agent that asked, and its whole tree is left out: the
 * agent runs up to four probes at once, and each of them used to see the
 * others, and their children, start and exit around it. Windows keeps a
 * parent's id after the parent exits and gives it to the next process, so a
 * parent id there proves descent only from a parent created before the child.
 */
export function processExitVerdict(profile, rows, ownPid = process.pid, { platform = process.platform, systemRoot = process.env.SystemRoot, agentPid = null, install = null, cwdOf = null, io = fs } = {}) {
  const names = NAMES[profile];
  if (!names || !Array.isArray(rows)) return { busy: true, unknown: true, reason: 'Process inspection is unavailable for this profile.' };
  const paths = platform === 'win32' ? path.win32 : path.posix;
  const fold = (value) => (platform === 'linux' ? value : value.toLowerCase());
  const patterns = names.map((name) => new RegExp(`(?:^|[\\s/\\\\"'])${name.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}(?:\\.exe|\\.app)?(?:$|[\\s/\\\\"'])`, platform === 'win32' ? 'i' : ''));
  const file = realFiles(io);
  const inside = (candidate, roots) => roots.some((root) => {
    const base = fold(paths.normalize(root)).replace(/[\\/]+$/, '');
    const target = fold(paths.normalize(candidate));
    return target === base || target.startsWith(`${base}${paths.sep}`);
  });
  const owned = install?.owned?.length ? install.owned : null;
  const links = new Set((install?.links ?? []).map((link) => fold(paths.normalize(link))));
  const foreign = install?.foreign ?? [];
  const descends = platform === 'win32'
    ? (parent, child) => typeof parent?.createdAt === 'string' && typeof child.createdAt === 'string' && parent.createdAt < child.createdAt
    : () => true;
  const excluded = Number.isInteger(agentPid) ? agentTree(rows, agentPid, ownPid, descends) : new Set();
  let uncertain = false;
  for (const row of rows) {
    if (platform === 'win32' && identifiedWindowsSystemProcess(row, rows, systemRoot)) continue;
    if (!Number.isInteger(row.pid) || row.pid <= 0) { uncertain = true; continue; }
    if (row.pid === ownPid || excluded.has(row.pid)) continue;
    // A zombie has exited: it runs nothing and holds no file open.
    if (platform !== 'win32' && typeof row.state === 'string' && row.state.startsWith('Z')) continue;
    const identity = identify(row, { paths, file, cwdOf });
    const where = identity.located.flatMap((candidate) => [candidate, file.real(candidate)]).filter(Boolean);
    let related;
    if (owned && where.length > 0) related = where.some((candidate) => inside(candidate, owned) || links.has(fold(paths.normalize(candidate))));
    else if (where.some((candidate) => inside(candidate, foreign))) related = false;
    else related = patterns.some((pattern) => identity.names.some((value) => pattern.test(value)));
    if (related) return { busy: true, unknown: false, reason: 'A related application or CLI session is still running.' };
    if (identity.uncertain) uncertain = true;
  }
  return uncertain
    ? { busy: true, unknown: true, reason: 'Some process identities could not be inspected; waiting for a reliable idle result.' }
    : { busy: false, unknown: false, reason: 'No related application or CLI session is running.' };
}

function readJson(file, io) {
  try {
    if (io.statSync(file).size > 1024 * 1024) return null;
    return JSON.parse(io.readFileSync(file, 'utf8'));
  } catch { return null; }
}

/**
 * Where the install a profile's update replaces lives, and where the other
 * profiles' applications live. An npm service's prefix and package come from
 * the agent that runs this probe: npm replaces <prefix>/lib/node_modules/<package>
 * (<prefix>\node_modules\<package> on Windows) and the bin links into it, and
 * nothing else. An application is the bundle its installer put in place.
 */
export function profileInstall(profile, { platform = process.platform, env = process.env, home = os.homedir(), io = fs } = {}) {
  const paths = platform === 'win32' ? path.win32 : path.posix;
  const file = realFiles(io);
  const owned = [];
  const links = [];
  const prefix = env.LEGIONCTL_NPM_PREFIX;
  const packageName = env.LEGIONCTL_NPM_PACKAGE;
  if (typeof prefix === 'string' && paths.isAbsolute(prefix) && typeof packageName === 'string' && /^(?:@[a-z0-9._~-]+\/)?[a-z0-9._~-]+$/i.test(packageName)) {
    const root = platform === 'win32' ? paths.join(prefix, 'node_modules', packageName) : paths.join(prefix, 'lib', 'node_modules', packageName);
    const realRoot = file.real(root);
    owned.push(root, ...(realRoot && realRoot !== root ? [realRoot] : []));
    const bin = readJson(paths.join(root, 'package.json'), io)?.bin;
    const commands = typeof bin === 'string' ? [packageName.split('/').at(-1)] : bin && typeof bin === 'object' ? Object.keys(bin) : [];
    for (const command of commands.filter((entry) => /^[A-Za-z0-9._-]+$/.test(entry))) {
      // A Windows shim is a script of its own that starts node on the package;
      // a POSIX link is the package's own only while it still points into it.
      if (platform === 'win32') links.push(...['', '.cmd', '.ps1'].map((suffix) => paths.join(prefix, `${command}${suffix}`)).filter((shim) => file.isFile(shim)));
      else {
        const link = paths.join(prefix, 'bin', command);
        const target = file.real(link);
        if (target && [root, realRoot].some((base) => base && (target === base || target.startsWith(`${base}/`)))) links.push(link);
      }
    }
  }
  const system = platform === 'darwin' ? 'mac' : platform === 'win32' ? 'windows' : platform;
  const bundles = (id) => desktopLocations(DESKTOP_APPLICATIONS[id], { platform: system, home, env })
    .filter((location) => (system === 'mac' ? file.real(location) && !file.isFile(location) : file.isFile(location)))
    .map((location) => (system === 'mac' ? location : paths.dirname(location)));
  const own = DESKTOP_APPLICATIONS[profile] ? bundles(profile) : [];
  owned.push(...own);
  const foreign = Object.keys(DESKTOP_APPLICATIONS).flatMap(bundles).filter((bundle) => !own.includes(bundle));
  // Every packaged Windows application runs from under WindowsApps, where
  // discovery does not look, and no CLI does. So a CLI profile never claims a
  // process from there: not the Store's ChatGPT for codex, nor Claude for claude.
  if (system === 'windows' && !DESKTOP_APPLICATIONS[profile] && typeof env.ProgramFiles === 'string' && paths.isAbsolute(env.ProgramFiles)) {
    foreign.push(paths.join(env.ProgramFiles, 'WindowsApps'));
  }
  return { owned, links, foreign: [...new Set(foreign)] };
}

/** The agent that asked, when it started this probe and says so. */
function requestingAgent(env, parentPid) {
  const pid = Number(env.LEGIONCTL_AGENT_PID);
  return Number.isInteger(pid) && pid > 1 && pid === parentPid ? pid : null;
}

function readPs(run, columns, pids = null) {
  const result = run('/bin/ps', ['-ww', ...(pids ? ['-p', pids.join(',')] : ['-ax']), ...columns.flatMap((column) => ['-o', `${column}=`])], { encoding: 'utf8', timeout: 2000, maxBuffer: 4 * 1024 * 1024 });
  const text = String(result.stdout ?? '').trim();
  // ps -p exits 1 when none of the processes asked about is left, which is an answer.
  if (result.error || (result.status !== 0 && !(pids && result.status === 1 && !text)) || (!text && !pids)) return null;
  const pattern = columns.length > 2 ? /^\s*(\d+)\s+(\d+)\s+(\S+)\s+(.*)$/ : /^\s*(\d+)\s+(.*)$/;
  const entries = new Map();
  for (const line of text ? text.split('\n') : []) {
    const found = pattern.exec(line);
    if (!found || entries.has(Number(found[1]))) return null;
    entries.set(Number(found[1]), columns.length > 2 ? { parentPid: Number(found[2]), state: found[3], name: found[4] } : { command: found[2] });
  }
  // Each short-lived ps child exists in its own snapshot only. Its PID comes
  // from spawnSync, so excluding it cannot hide a product session.
  if (Number.isInteger(result.pid)) entries.delete(result.pid);
  return entries;
}

/**
 * A Mac's processes, from two ps reads: comm with the parent and state, then
 * the command line, since ps cannot quote and comm keeps its spaces only as
 * the last column. A process both reads saw, with a command line that starts
 * with its comm, is one consistent row. One that only the first read saw had
 * exited by the second. One that started, or replaced its program, between
 * them is read again on its own, and dropped if it has gone by then.
 */
function darwinRows(run) {
  const NAME_COLUMNS = ['pid', 'ppid', 'stat', 'comm'];
  const consistent = (seen, command) => seen && command !== undefined && (command === seen.name || command.startsWith(`${seen.name} `));
  const names = readPs(run, NAME_COLUMNS);
  const commands = readPs(run, ['pid', 'args']);
  if (!names || !commands) return null;
  const rows = [];
  const unsettled = [];
  for (const pid of new Set([...names.keys(), ...commands.keys()])) {
    const seen = names.get(pid);
    const command = commands.get(pid)?.command;
    if (consistent(seen, command)) rows.push({ pid, ...seen, command });
    else if (!seen || command !== undefined) unsettled.push(pid);
  }
  if (unsettled.length === 0) return rows;
  const namesAgain = readPs(run, NAME_COLUMNS, unsettled);
  const commandsAgain = readPs(run, ['pid', 'args'], unsettled);
  if (!namesAgain || !commandsAgain) return null;
  for (const pid of unsettled) {
    const seen = namesAgain.get(pid);
    const command = commandsAgain.get(pid)?.command;
    if (command === undefined) continue;
    rows.push(consistent(seen, command) ? { pid, ...seen, command } : { pid, parentPid: seen?.parentPid, state: seen?.state, name: null, command });
  }
  return rows;
}

/**
 * One Linux process from /proc: stat carries comm, the parent, the state and
 * the flags in one read, and cmdline is argv exactly, NUL between the words.
 * A process gone in the middle of the read is gone, and so are zombies and
 * kernel threads, which run no program. A process with no argv that is none of
 * those is between exit and zombie, or hides its argv; it is read once more.
 */
function linuxProcess(pid, io) {
  const gone = (error) => error?.code === 'ENOENT' || error?.code === 'ESRCH';
  const readStat = () => {
    const stat = String(io.readFileSync(`/proc/${pid}/stat`, 'utf8'));
    const open = stat.indexOf('(');
    const close = stat.lastIndexOf(')');
    if (open < 0 || close < open) return null;
    const [state, parentPid, , , , , flags] = stat.slice(close + 1).trim().split(/\s+/);
    return { name: stat.slice(open + 1, close), state, parentPid: Number(parentPid), flags: Number(flags) };
  };
  let stat;
  let argv;
  try {
    stat = readStat();
    argv = String(io.readFileSync(`/proc/${pid}/cmdline`)).split('\0');
  } catch (error) { return gone(error) ? null : { pid, name: null, command: null }; }
  if (!stat) return { pid, name: null, command: null };
  if (/^[ZXx]/.test(stat.state) || (stat.flags & KERNEL_THREAD) !== 0) return null;
  if (argv.at(-1) === '') argv.pop();
  if (argv.length === 0) {
    try { const again = readStat(); if (!again || /^[ZXx]/.test(again.state)) return null; } catch (error) { if (gone(error)) return null; }
  }
  let executablePath = null;
  try { executablePath = String(io.readlinkSync(`/proc/${pid}/exe`)).replace(/ \(deleted\)$/, ''); } catch { /* another user's process, or gone */ }
  return { pid, parentPid: stat.parentPid, state: stat.state, name: stat.name, argv, command: argv.join(' '), executablePath };
}

function linuxRows(io) {
  return io.readdirSync('/proc').filter((entry) => /^\d+$/.test(entry)).map((entry) => linuxProcess(Number(entry), io)).filter(Boolean);
}

export function inspectProfileProcesses(profile, { platform = process.platform, run = spawnSync, io = fs, env = process.env, home = os.homedir(), parentPid = process.ppid } = {}) {
  if (!NAMES[profile]) return processExitVerdict(profile, null);
  try {
    let rows;
    let cwdOf = null;
    if (platform === 'win32') {
      const ownerCandidates = [...WINDOWS_SYSTEM_NAMES, ...WINDOWS_KERNEL_NAMES, ...WINDOWS_ISOLATED_SERVICE_IDENTITIES.keys(), 'vmmem', 'vmmemwsl'];
      const script = `$ErrorActionPreference='Stop'; $names=@(${ownerCandidates.map((name) => `'${name}'`).join(',')}); ` +
        `@(Get-CimInstance Win32_Process | ForEach-Object { $sid=$null; $created=$null; ` +
        `if ($_.CreationDate) { $created=$_.CreationDate.ToUniversalTime().ToString('o') }; ` +
        `if ([string]::IsNullOrEmpty($_.CommandLine) -and $names -contains $_.Name) { ` +
        `try { $owner=Invoke-CimMethod -InputObject $_ -MethodName GetOwnerSid; if ($owner.ReturnValue -eq 0) { $sid=$owner.Sid } } catch {} }; ` +
        `[PSCustomObject]@{ProcessId=$_.ProcessId;ParentProcessId=$_.ParentProcessId;SessionId=$_.SessionId;Name=$_.Name;CommandLine=$_.CommandLine;ExecutablePath=$_.ExecutablePath;OwnerSid=$sid;CreatedAt=$created} }) | ConvertTo-Json -Compress`;
      const result = run('powershell.exe', ['-NoProfile', '-NonInteractive', '-Command', script], { encoding: 'utf8', timeout: 4000, maxBuffer: 4 * 1024 * 1024, windowsHide: true });
      if (result.status !== 0 || result.error) return processExitVerdict(profile, null);
      const entries = JSON.parse(result.stdout);
      if (!Array.isArray(entries)) return processExitVerdict(profile, null);
      // CreatedAt is the round-trip format, UTC to the tick, so creation times compare as text.
      rows = entries.map((row) => ({ pid: row.ProcessId, parentPid: row.ParentProcessId, sessionId: row.SessionId, name: row.Name, command: row.CommandLine, executablePath: row.ExecutablePath, ownerSid: row.OwnerSid, createdAt: row.CreatedAt }));
    } else if (platform === 'darwin') rows = darwinRows(run);
    else if (platform === 'linux') {
      rows = linuxRows(io);
      cwdOf = (pid) => { try { return String(io.readlinkSync(`/proc/${pid}/cwd`)); } catch { return null; } };
    } else return processExitVerdict(profile, null);
    if (!rows) return processExitVerdict(profile, null);
    return processExitVerdict(profile, rows, process.pid, { platform, agentPid: requestingAgent(env, parentPid), install: profileInstall(profile, { platform, env, home, io }), cwdOf, io });
  } catch { return processExitVerdict(profile, null); }
}

export function readProfileVersion(executable, { run = spawnSync, cwd } = {}) {
  if (typeof executable !== 'string' || !path.isAbsolute(executable)) return null;
  const result = run(executable, ['--version'], { encoding: 'utf8', timeout: 4000, maxBuffer: 64 * 1024, windowsHide: true, cwd });
  if (result.status !== 0 || result.error) return null;
  return /(?:^|\s)(\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?)(?=\s|$)/.exec(String(result.stdout))?.[1] ?? null;
}

/** The fixed publisher feed used by the official native installer. */
export async function latestClaudeVersion(channel, { request = fetch } = {}) {
  if (!['latest', 'stable'].includes(channel)) return null;
  try {
    const response = await request(`https://downloads.claude.ai/claude-code-releases/${channel}`, { signal: AbortSignal.timeout(5000), redirect: 'error' });
    if (!response.ok || !response.body) return null;
    const reader = response.body.getReader();
    let total = 0;
    const chunks = [];
    try {
      while (true) {
        const { done, value } = await reader.read();
        if (done) break;
        total += value.byteLength;
        if (total > 4096) { await reader.cancel(); return null; }
        chunks.push(Buffer.from(value));
      }
    } finally { reader.releaseLock(); }
    const version = Buffer.concat(chunks).toString('utf8').trim();
    return /^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/.test(version) ? version : null;
  } catch { return null; }
}

export async function runNativeClaudeProbe(verb, { executable, home, expectedPolicy }, { resolvePolicy = resolveClaudeUpdatePolicy, run = spawnSync, latest = latestClaudeVersion, nativeVersion = nativeClaudeVersionAt } = {}) {
  const ownedVersion = nativeVersion(executable);
  const supported = compareProfileVersions(ownedVersion, '2.1.251');
  if (supported === null || supported < 0) return { ok: false, message: 'The selected executable is no longer a supported native installation; review the profile again.' };
  const policy = resolvePolicy({ home, executable });
  if (!policy.ok || policy.hash !== expectedPolicy || typeof executable !== 'string' || !path.isAbsolute(executable)) {
    return { ok: false, message: 'Native update policy changed or could not be verified; review the profile again.' };
  }
  if (verb === 'native-version') {
    const version = readProfileVersion(executable, { cwd: home, run });
    return version === ownedVersion ? { ok: true, version } : { ok: false, message: 'The installed native version could not be read.' };
  }
  if (verb === 'native-latest') {
    const installed = readProfileVersion(executable, { cwd: home, run });
    const version = await latest(policy.channel);
    return installed === ownedVersion && claudeTargetAllowed(policy, version, installed) ? { ok: true, version } : { ok: false, message: 'No verified target satisfies the current channel and version constraints.' };
  }
  if (verb !== 'native-update') return { ok: false, message: 'Unsupported native probe.' };
  try {
    const installed = readProfileVersion(executable, { cwd: home, run });
    const target = await latest(policy.channel);
    if (installed !== ownedVersion || !claudeTargetAllowed(policy, target, installed)) return { ok: false, message: 'No verified target satisfies the current channel and version constraints.' };
    const currentPolicy = resolvePolicy({ home, executable });
    if (!currentPolicy.ok || currentPolicy.hash !== expectedPolicy || nativeVersion(executable) !== ownedVersion) return { ok: false, message: 'Native update policy or installation changed; review the profile again.' };
    const result = run(executable, ['update'], { cwd: home, encoding: 'utf8', timeout: 540000, maxBuffer: 1024 * 1024, windowsHide: true });
    if (result.status !== 0 || result.error) return { ok: false, message: 'The official native updater did not complete successfully.' };
    const afterPolicy = resolvePolicy({ home, executable });
    const observed = readProfileVersion(executable, { cwd: home, run });
    return afterPolicy.ok && afterPolicy.hash === expectedPolicy && observed === target && nativeVersion(executable) === target
      ? { ok: true }
      : { ok: false, message: 'The native update did not match the verified target and policy; refresh before retrying.' };
  } catch { return { ok: false, message: 'The official native updater could not be started.' }; }
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const [verb, profile, ...extra] = process.argv.slice(2);
  if (verb === 'desktop-version' && ['claude-desktop', 'codex-desktop', 'chatgpt-desktop', 'antigravity'].includes(profile) && extra.length === 2) {
    const metadata = readDesktopMetadata(extra[0]);
    if (!metadata || metadata.identity !== extra[1]) { process.stderr.write('The installed application metadata could not be verified.\n'); process.exitCode = 1; }
    else process.stdout.write(`${metadata.version}\n`);
  } else if (['native-version', 'native-latest', 'native-update'].includes(verb) && profile === 'claude-code' && extra.length === 3) {
    const [executable, home, expectedPolicy] = extra;
    const result = await runNativeClaudeProbe(verb, { executable, home, expectedPolicy });
    if (!result.ok) { process.stderr.write(`${result.message}\n`); process.exitCode = 1; }
    else if (result.version) process.stdout.write(`${result.version}\n`);
  } else if (verb !== 'busy' || extra.length || !NAMES[profile]) {
    process.stderr.write('Unsupported profile probe.\n'); process.exitCode = 1;
  } else process.stdout.write(`${JSON.stringify(inspectProfileProcesses(profile))}\n`);
}
