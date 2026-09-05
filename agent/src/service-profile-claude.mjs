// Resolve only update-related native Claude settings. Unhandled policy sources
// are explicit refusals; discovery never runs a policy helper or updater.
import crypto from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';

const object = (value) => value !== null && typeof value === 'object' && !Array.isArray(value);
const VERSION = /^\d+\.\d+\.\d+$/;
const KEYS = ['autoUpdatesChannel', 'minimumVersion', 'requiredMinimumVersion', 'requiredMaximumVersion'];
const ENV_KEYS = ['DISABLE_UPDATES', 'CLAUDE_CONFIG_DIR', 'ANTHROPIC_BASE_URL', 'CLAUDE_CODE_USE_BEDROCK', 'CLAUDE_CODE_USE_VERTEX', 'CLAUDE_CODE_USE_FOUNDRY'];

export function nativeClaudeVersionAt(executable, { io = fs } = {}) {
  try {
    const resolved = io.realpathSync(executable).split(path.sep).join('/');
    const match = /\/\.local\/share\/claude\/versions\/(\d+\.\d+\.\d+)$/.exec(resolved);
    return match?.[1] ?? null;
  } catch { return null; }
}

export function compareProfileVersions(a, b) {
  if (!VERSION.test(a ?? '') || !VERSION.test(b ?? '')) return null;
  const left = a.split('.').map(Number); const right = b.split('.').map(Number);
  for (let i = 0; i < 3; i += 1) { if (left[i] !== right[i]) return left[i] < right[i] ? -1 : 1; }
  return 0;
}

function bool(value) {
  if (value === undefined) return false;
  if (typeof value !== 'string') throw new Error('An update-related environment setting has an unsupported value.');
  if (/^(1|true)$/i.test(value)) return true;
  if (/^(0|false)$/i.test(value)) return false;
  throw new Error('An update-related environment setting has an unsupported value.');
}

/** The diagnostic's explicit remote-policy result leaves this bounded query. */
export function readClaudeRemotePolicy(executable, { home, run = spawnSync } = {}) {
  try {
    if (typeof executable !== 'string' || !path.isAbsolute(executable)) return null;
    const result = run(executable, ['doctor'], { cwd: home, encoding: 'utf8', timeout: 3000, maxBuffer: 262144, windowsHide: true });
    if (result.error || result.status !== 0) return null;
    const output = String(result.stdout).replace(/\x1b\[[0-?]*[ -/]*[@-~]/g, '');
    const lines = output.split('\n').map((line) => line.trim()).filter((line) => line.startsWith('Managed settings (remote):'));
    return lines.length === 1 && lines[0] === 'Managed settings (remote): not fetched — requires an Enterprise or Team subscription' ? 'not-eligible' : null;
  } catch { return null; }
}

export function resolveClaudeUpdatePolicy({ home = os.homedir(), username = os.userInfo().username, platform = process.platform, env = process.env, io = fs, executable, inspectRemote = readClaudeRemotePolicy } = {}) {
  try {
    if (!path.isAbsolute(home)) throw new Error('The updater requires an absolute home directory.');
    if (!['darwin', 'linux'].includes(platform) || env.WSL_DISTRO_NAME) throw new Error('Native policy discovery cannot yet verify Windows or inherited WSL registry policy.');
    if (platform === 'linux') {
      let kernel;
      try { kernel = io.readFileSync('/proc/version', 'utf8'); } catch { throw new Error('The Linux policy environment cannot be distinguished from WSL.'); }
      if (typeof kernel !== 'string' || kernel.length > 65536 || /microsoft|wsl/i.test(kernel)) throw new Error('Inherited WSL registry policy requires explicit verification.');
    }
    const read = (file) => {
      try {
        const stat = io.statSync(file);
        if (!stat.isFile() || stat.size > 1024 * 1024) throw new Error('invalid');
        const value = JSON.parse(io.readFileSync(file, 'utf8'));
        if (!object(value)) throw new Error('invalid');
        return value;
      } catch (error) {
        if (error?.code === 'ENOENT') return {};
        throw new Error('An update policy file is unreadable or invalid.');
      }
    };
    const exists = (file) => { try { io.statSync(file); return true; } catch (error) { if (error?.code === 'ENOENT') return false; throw new Error('A policy source cannot be inspected.'); } };
    const configDirectory = env.CLAUDE_CONFIG_DIR || path.join(home, '.claude');
    if (typeof configDirectory !== 'string' || !path.isAbsolute(configDirectory)) throw new Error('CLAUDE_CONFIG_DIR must be absolute before a native profile can be enabled.');
    const user = read(path.join(configDirectory, 'settings.json'));
    const project = path.resolve(configDirectory) === path.join(home, '.claude') ? {} : read(path.join(home, '.claude', 'settings.json'));
    const local = read(path.join(home, '.claude', 'settings.local.json'));
    const systemDirectory = platform === 'darwin' ? '/Library/Application Support/ClaudeCode' : '/etc/claude-code';
    let managed = read(path.join(systemDirectory, 'managed-settings.json'));
    const fragments = path.join(systemDirectory, 'managed-settings.d');
    if (exists(fragments)) {
      const names = io.readdirSync(fragments).filter((name) => !name.startsWith('.') && name.endsWith('.json')).sort();
      if (names.length > 100) throw new Error('There are too many managed policy fragments to inspect safely.');
      if (managed.env !== undefined && !object(managed.env)) throw new Error('A settings environment block is invalid.');
      for (const name of names) {
        const fragment = read(path.join(fragments, name));
        if (fragment.env !== undefined && !object(fragment.env)) throw new Error('A settings environment block is invalid.');
        managed = { ...managed, ...fragment, env: { ...(managed.env ?? {}), ...(fragment.env ?? {}) } };
      }
    }
    if (platform === 'darwin') {
      // The publisher's settings resolver reads these two managed-preference
      // files. A custom home directory does not change the account name.
      if (typeof username !== 'string' || !username || /[/\\\0]/.test(username)) throw new Error('The account name for managed preferences cannot be verified.');
      for (const file of [
        path.join('/Library/Managed Preferences', username, 'com.anthropic.claudecode.plist'),
        '/Library/Managed Preferences/com.anthropic.claudecode.plist',
      ]) if (exists(file)) throw new Error('macOS preference policy is present; its effective managed values need explicit verification.');
    }
    for (const source of [user, project, local, managed]) {
      if (source.policyHelper) throw new Error('A policy helper controls native updates; it will not be executed during discovery.');
      if (source.env !== undefined && !object(source.env)) throw new Error('A settings environment block is invalid.');
      if (['HOME', 'USERPROFILE', 'APPDATA', 'XDG_CONFIG_HOME'].some((key) => source.env?.[key] !== undefined)) throw new Error('Settings redirect a home or policy directory; resolve that environment before enabling updates.');
      if (source.forceLoginMethod === 'gateway' || source.forceLoginGatewayUrl || source.env?.CLAUDE_CODE_GATEWAY_TOKEN_FILE_DESCRIPTOR) throw new Error('Gateway-managed policy requires a verified last-approved snapshot before enabling native updates.');
    }
    if (env.CLAUDE_CODE_GATEWAY_TOKEN_FILE_DESCRIPTOR) throw new Error('Gateway-managed policy requires a verified last-approved snapshot before enabling native updates.');
    if (managed.managedSourcesBehavior !== undefined && !['first-wins', 'merge'].includes(managed.managedSourcesBehavior)) throw new Error('The managed source merge mode is unsupported.');
    const sources = [user, project, local, managed];
    const relevant = {};
    const effectiveEnv = { ...env };
    for (const source of sources) {
      for (const key of KEYS) {
        if (key.startsWith('required') && source !== managed) continue;
        if (source[key] !== undefined) relevant[key] = source[key];
      }
      for (const key of ENV_KEYS) if (source.env?.[key] !== undefined) {
        if (key === 'CLAUDE_CONFIG_DIR' && source !== user && source !== managed) continue;
        effectiveEnv[key] = source.env[key];
      }
    }
    if (effectiveEnv.CLAUDE_CONFIG_DIR && (typeof effectiveEnv.CLAUDE_CONFIG_DIR !== 'string' || !path.isAbsolute(effectiveEnv.CLAUDE_CONFIG_DIR) || path.resolve(effectiveEnv.CLAUDE_CONFIG_DIR) !== path.resolve(configDirectory))) throw new Error('Settings redirect the configuration directory; resolve that policy before enabling updates.');
    if (bool(effectiveEnv.DISABLE_UPDATES)) throw new Error('DISABLE_UPDATES blocks both manual and scheduled native updates.');
    const channel = relevant.autoUpdatesChannel ?? 'latest';
    if (!['latest', 'stable'].includes(channel)) throw new Error('The effective native update channel is unsupported.');
    for (const key of KEYS.slice(1)) if (relevant[key] !== undefined && !VERSION.test(relevant[key])) throw new Error('A configured version constraint cannot be compared safely.');
    const remotePolicy = inspectRemote(executable, { home });
    if (remotePolicy !== 'not-eligible' || Object.keys(read(path.join(configDirectory, 'remote-settings.json'))).length || managed.forceRemoteSettingsRefresh) {
      throw new Error('Remote managed policy requires a verified last-approved snapshot or explicit no-policy diagnostic before enabling native updates.');
    }
    const policy = {
      home, configDirectory, channel, minimumVersion: relevant.minimumVersion ?? null,
      requiredMinimumVersion: relevant.requiredMinimumVersion ?? null, requiredMaximumVersion: relevant.requiredMaximumVersion ?? null,
      remotePolicy,
    };
    return { ok: true, ...policy, hash: crypto.createHash('sha256').update(JSON.stringify(policy)).digest('hex') };
  } catch (error) { return { ok: false, message: error.message }; }
}

export function claudeTargetAllowed(policy, target, installed = null) {
  if (!policy?.ok || !VERSION.test(target ?? '')) return false;
  for (const minimum of [policy.minimumVersion, policy.requiredMinimumVersion, installed]) {
    if (minimum && (compareProfileVersions(target, minimum) === null || compareProfileVersions(target, minimum) < 0)) return false;
  }
  return !policy.requiredMaximumVersion || compareProfileVersions(target, policy.requiredMaximumVersion) <= 0;
}
