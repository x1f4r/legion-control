// Host-local discovery for explicitly selected service profiles. Discovery
// reads package metadata and bounded policy diagnostics; it never starts
// a session, installs a product, or runs an updater.
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { detectPlatform } from './config.mjs';
import { resolveClaudeUpdatePolicy, compareProfileVersions, nativeClaudeVersionAt } from './service-profile-claude.mjs';
import { readDesktopMetadata } from './service-profile-desktop.mjs';

const CLI = [
  { id: 'claude-code', name: 'Claude Code', executable: 'claude', package: '@anthropic-ai/claude-code' },
  { id: 'codex-cli', name: 'Codex CLI', executable: 'codex', package: '@openai/codex' },
  { id: 'opencode', name: 'OpenCode', executable: 'opencode', package: 'opencode-ai' },
];

function fileExists(file, io) { try { return io.statSync(file).isFile(); } catch { return false; } }
function directoryExists(file, io) { try { return io.statSync(file).isDirectory(); } catch { return false; } }
function real(file, io) { try { return io.realpathSync(file); } catch { return file; } }
function json(file, io) {
  try {
    if (io.statSync(file).size > 1024 * 1024) return null;
    return JSON.parse(io.readFileSync(file, 'utf8'));
  } catch { return null; }
}

function findExecutable(name, env, platform, io) {
  const suffixes = platform === 'windows' ? ['.exe', '.cmd', '.bat', ''] : [''];
  for (const directory of String(env.PATH ?? '').split(platform === 'windows' ? ';' : ':')) {
    if (!directory || !path.isAbsolute(directory)) continue;
    for (const suffix of suffixes) {
      const candidate = path.join(directory, `${name}${suffix}`);
      if (!fileExists(candidate, io)) continue;
      try { io.accessSync(candidate, fs.constants.X_OK); return candidate; } catch { /* not executable */ }
    }
  }
  return null;
}

function npmInstall(executable, packageName, platform, io) {
  const resolved = real(executable, io);
  const candidates = [];
  let directory = path.dirname(resolved);
  for (let depth = 0; depth < 8; depth += 1) {
    candidates.push(directory);
    const parent = path.dirname(directory); if (parent === directory) break; directory = parent;
  }
  const launcherDir = path.dirname(executable);
  let shim = null;
  if (platform === 'windows' && /\.cmd$/i.test(executable)) {
    try { if (io.statSync(executable).size <= 65536) shim = io.readFileSync(executable, 'utf8').toLowerCase(); } catch { /* unknown shim */ }
    if (shim) candidates.push(path.join(launcherDir, 'node_modules', packageName));
  }
  for (const root of candidates) {
    const metadata = json(path.join(root, 'package.json'), io);
    if (metadata?.name !== packageName || typeof metadata.version !== 'string') continue;
    const normalized = root.split(path.sep).join('/');
    const suffix = `/node_modules/${packageName}`;
    if (!normalized.endsWith(suffix)) continue;
    if (root === path.join(launcherDir, 'node_modules', packageName)) {
      const binName = path.basename(executable).replace(/\.cmd$/i, '');
      const binEntry = typeof metadata.bin === 'string' ? metadata.bin : metadata.bin?.[binName];
      if (typeof binEntry !== 'string' || path.isAbsolute(binEntry) || binEntry.split(/[\\/]/).includes('..')) continue;
      const reference = path.join('node_modules', packageName, binEntry).replaceAll('/', '\\').toLowerCase();
      if (!shim?.includes(`%dp0%\\${reference}`) && !shim?.includes(`%~dp0${reference}`)) continue;
    }
    let prefix = root.slice(0, root.length - suffix.length);
    if (platform !== 'windows' && path.basename(prefix) === 'lib') prefix = path.dirname(prefix);
    return { prefix, version: metadata.version };
  }
  return null;
}

function baseService(id, name) {
  return {
    id, name, process: { type: 'none' }, health: { type: 'none' },
    busy: { type: 'command', command: [process.execPath, path.join(import.meta.dirname, 'service-profile-probe.mjs'), 'busy', id], timeoutSeconds: 6 },
    updates: { automatic: false },
  };
}

function directNpmRuntime(prefix, io, npm) {
  const nodeDirectory = path.dirname(process.execPath);
  return [
    path.join(nodeDirectory, 'node_modules', 'npm', 'bin', 'npm-cli.js'),
    path.join(nodeDirectory, '..', 'lib', 'node_modules', 'npm', 'bin', 'npm-cli.js'),
    path.join(prefix, 'node_modules', 'npm', 'bin', 'npm-cli.js'),
    ...[npm, real(npm, io)].map((launcher) => path.join(path.dirname(launcher), 'node_modules', 'npm', 'bin', 'npm-cli.js')),
  ].some((candidate) => fileExists(candidate, io));
}

function desktopProfile(id, name, names, { platform, home, env, io, readDesktop = readDesktopMetadata }) {
  let candidates = [];
  if (platform === 'mac') candidates = names.flatMap((app) => [path.join('/Applications', `${app}.app`), path.join(home, 'Applications', `${app}.app`)]);
  if (platform === 'windows') {
    const locations = [env.LOCALAPPDATA, env.ProgramFiles, env['ProgramFiles(x86)']].filter(Boolean);
    candidates = names.flatMap((app) => locations.flatMap((location) => [path.join(location, app, `${app}.exe`), path.join(location, 'Programs', app, `${app}.exe`)]));
  }
  const found = candidates.find((candidate) => platform === 'mac' ? directoryExists(candidate, io) : fileExists(candidate, io));
  const metadata = found ? readDesktop(found, { platform: platform === 'mac' ? 'darwin' : 'win32', io }) : null;
  return {
    id, name, platform, detected: Boolean(found), availability: found ? 'manual' : 'not-installed',
    message: found ? 'The application is installed. Its built-in updater has no verified unattended command for this install; update through the application.' : 'No supported local application installation was detected.',
    updateMethod: found ? 'application-updater' : null,
    service: metadata ? {
      ...baseService(id, name), kind: 'command',
      installedVersion: [process.execPath, path.join(import.meta.dirname, 'service-profile-probe.mjs'), 'desktop-version', id, found, metadata.identity],
    } : null,
  };
}

export function discoverServiceProfiles({ platform = detectPlatform(), home = os.homedir(), env = process.env, io = fs, configuration = {}, readDesktop = readDesktopMetadata } = {}) {
  if (!['mac', 'linux', 'windows'].includes(platform)) return [];
  const npm = findExecutable('npm', env, platform, io);
  const profiles = CLI.map((product) => {
    const executable = findExecutable(product.executable, env, platform, io);
    const install = executable ? npmInstall(executable, product.package, platform, io) : null;
    const existing = (Array.isArray(configuration.services) ? configuration.services : []).find((service) => service?.kind === 'npm' && service.package === product.package && typeof service.npmPrefix === 'string' && install && real(service.npmPrefix, io) === real(install.prefix, io));
    const channel = typeof existing?.channel === 'string' && existing.channel ? existing.channel : install?.version.includes('-') ? null : 'latest';
    const runtimeAvailable = Boolean(install && (platform !== 'windows' || (npm && directNpmRuntime(install.prefix, io, npm))));
    const available = Boolean(install && npm && channel && runtimeAvailable);
    const nativeClaude = product.id === 'claude-code' && executable && nativeClaudeVersionAt(executable, { io });
    if (nativeClaude) {
      const version = path.basename(real(executable, io));
      const modern = compareProfileVersions(version, '2.1.251');
      const policy = modern !== null && modern >= 0
        ? resolveClaudeUpdatePolicy({ home, executable, platform: platform === 'mac' ? 'darwin' : platform === 'windows' ? 'win32' : platform, env, io })
        : { ok: false, message: 'This native version predates the update policy rules supported by the profile.' };
      const invoke = (verb) => [process.execPath, path.join(import.meta.dirname, 'service-profile-probe.mjs'), verb, product.id, executable, home, policy.hash];
      return {
        id: product.id, name: product.name, platform, detected: true, availability: policy.ok ? 'available' : 'unavailable',
        message: policy.ok ? `Native ${policy.channel} channel resolved for the home directory. Automatic maintenance is off; all Claude processes must exit and the exact target version must be observed.` : policy.message,
        updateMethod: 'claude-native',
        service: policy.ok ? { ...baseService(product.id, product.name), kind: 'command', channel: policy.channel, installedVersion: invoke('native-version'), latestVersion: invoke('native-latest'), update: invoke('native-update'), requireVersionMatch: true, versionPattern: '^\\d+\\.\\d+\\.\\d+$' } : null,
      };
    }
    return {
      id: product.id, name: product.name, platform, detected: Boolean(executable),
      availability: available ? 'available' : executable ? 'manual' : 'not-installed',
      message: available
        ? 'A matching npm installation was detected. Review the draft before saving; automatic maintenance is off and every related process must exit before an update.'
        : install && !runtimeAvailable ? 'The npm package is installed, but this agent cannot locate a direct npm JavaScript runtime. Resolve that runtime before provisioning an updater.'
          : nativeClaude ? 'Native installation detected. The official manual updater is claude update; automatic provisioning awaits a verified latest-version source for this update channel.'
          : executable ? 'Executable detected, but its install method cannot be safely provisioned automatically. Keep its current package manager or official updater.'
            : 'This CLI was not found in the agent session PATH. Nothing will be installed automatically.',
      updateMethod: available ? 'npm' : nativeClaude ? 'claude-native' : null,
      service: available ? { ...baseService(product.id, product.name), kind: 'npm', package: product.package, npmPrefix: install.prefix, channel, allowScripts: product.id === 'opencode' ? ['opencode-ai'] : [] } : null,
    };
  });
  profiles.push(desktopProfile('claude-desktop', 'Claude desktop', ['Claude'], { platform, home, env, io, readDesktop }));
  // Current Codex and ChatGPT desktop are one application. One catalog row
  // prevents two scheduled profiles from trying to update the same bundle.
  profiles.push(desktopProfile('codex-desktop', 'Codex / ChatGPT desktop', ['ChatGPT', 'Codex'], { platform, home, env, io, readDesktop }));
  profiles.push(desktopProfile('antigravity', 'Antigravity', ['Antigravity'], { platform, home, env, io, readDesktop }));
  profiles.push({ id: 'grok-cli', name: 'GrokCLI', platform, detected: false, availability: 'unavailable', message: 'Several unrelated tools use this name. Select the exact project and distribution before provisioning an updater.', updateMethod: null, service: null });
  return profiles;
}
