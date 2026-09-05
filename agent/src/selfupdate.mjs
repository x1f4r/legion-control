// Signed, bounded and crash-recoverable agent replacement.

import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { AGENT_VERSION, basePath, incomingDir, runCommand } from './config.mjs';
import { CONTRACT_VERSION, REASON } from './contract.mjs';
import { stageFiles, verifyAgentArchive, verifyAgentTree, MAX_ARCHIVE_BYTES } from './archive.mjs';
import { commitInstallSwap, commitRollbackSwap, recoverAgentSwap, swapPaths } from './agent-swap.mjs';
import { log, note } from './log.mjs';
import { withOperation } from './operate.mjs';
import { readOperation, recordPhase } from './operations.mjs';
import { trustFingerprint } from './trust.mjs';

export const STDIN_IDLE_TIMEOUT_MS = 15_000;
export const STDIN_TOTAL_TIMEOUT_MS = 120_000;

export function agentDir(base = basePath()) { return path.join(base, 'agent'); }
export function stagingDir(base = basePath()) { return path.join(base, 'agent.new'); }
export function previousDir(base = basePath()) { return path.join(base, 'agent.prev'); }

export function treeVersion(dir) {
  try {
    const parsed = JSON.parse(fs.readFileSync(path.join(dir, 'package.json'), 'utf8'));
    return { version: typeof parsed.version === 'string' ? parsed.version : null, path: dir };
  } catch {
    return { version: null, path: dir };
  }
}

const digest = (bytes) => crypto.createHash('sha256').update(bytes).digest('hex');

function manifestShape(verified) {
  if (!verified?.ok) return null;
  return {
    schema: verified.manifest.schema,
    version: verified.manifest.version,
    contract: verified.manifest.contract ?? null,
    files: verified.entryCount,
    signatureVerified: true,
    keyFingerprint: trustFingerprint(),
  };
}

/** A manifest is only labelled verified after its signature and every file pass. */
export function installedManifest(dir) { return manifestShape(verifyAgentTree(dir)); }

export function selfTest(dir, { timeoutMs = 60_000, expectedVersion = null, expectedContract = null } = {}) {
  const entry = path.join(dir, 'src', 'index.mjs');
  if (!fs.existsSync(entry)) return { ok: false, exitCode: null, output: `${entry} does not exist` };
  const result = runCommand(process.execPath, [entry, 'version', '--check'], { timeoutMs });
  const output = (result.stdout || result.stderr || '').trim().split('\n').slice(-1)[0] ?? '';
  if (!result.ok) return { ok: false, exitCode: result.code, output: output || 'the staged agent did not start' };
  try {
    const parsed = JSON.parse(output);
    if (parsed.ok !== true || !Number.isInteger(parsed.contract)) return { ok: false, exitCode: result.code, output };
    if (parsed.selfTest?.ok !== true) return { ok: false, exitCode: result.code, output: `the staged agent did not pass its module and configuration checks: ${output}` };
    if (expectedVersion !== null && parsed.agentVersion !== expectedVersion) {
      return { ok: false, exitCode: result.code, output: `self-test reported agent ${JSON.stringify(parsed.agentVersion)}, signed manifest requires ${expectedVersion}` };
    }
    if (expectedContract !== null && parsed.contract !== expectedContract) {
      return { ok: false, exitCode: result.code, output: `self-test reported contract ${parsed.contract}, signed manifest requires ${expectedContract}` };
    }
  } catch {
    return { ok: false, exitCode: result.code, output: output || 'the staged agent printed something that is not JSON' };
  }
  return { ok: true, exitCode: 0, output };
}

function removeTree(dir) {
  try { fs.rmSync(dir, { recursive: true, force: true }); } catch { /* a later stage reports it */ }
}

function closeInput(input) {
  try { input.pause?.(); } catch { /* closing continues */ }
  try { input.destroy?.(); } catch { /* listeners are removed below */ }
  try { input.unref?.(); } catch { /* not every readable has an fd */ }
}

/** Read a bundle before taking the mutation lock, with cap, idle and total deadlines. */
export async function readBundle({ from = null, stdin = false, input = process.stdin, idleTimeoutMs = STDIN_IDLE_TIMEOUT_MS, totalTimeoutMs = STDIN_TOTAL_TIMEOUT_MS } = {}) {
  if (from) {
    try {
      const stat = fs.statSync(from);
      if (!stat.isFile()) return { ok: false, reasonCode: REASON.badArgument, error: `${from} is not a file` };
      if (stat.size > MAX_ARCHIVE_BYTES) return { ok: false, reasonCode: REASON.badArgument, error: `${from} is larger than ${MAX_ARCHIVE_BYTES} bytes` };
      return { ok: true, bytes: fs.readFileSync(from), source: from };
    } catch (error) {
      return { ok: false, reasonCode: REASON.badArgument, error: `${from} could not be read: ${error.message}` };
    }
  }
  if (!stdin) return { ok: false, reasonCode: REASON.badArgument, error: 'give either --from PATH or --stdin' };

  return new Promise((resolve) => {
    const chunks = [];
    let size = 0;
    let settled = false;
    let idleTimer = null;
    let totalTimer = null;
    const cleanup = () => {
      if (idleTimer) clearTimeout(idleTimer);
      if (totalTimer) clearTimeout(totalTimer);
      input.off?.('data', onData);
      input.off?.('end', onEnd);
      input.off?.('error', onError);
    };
    const finish = (value, close = false) => {
      if (settled) return;
      settled = true;
      cleanup();
      if (close) closeInput(input);
      resolve(value);
    };
    const armIdle = () => {
      if (idleTimer) clearTimeout(idleTimer);
      idleTimer = setTimeout(() => finish({ ok: false, reasonCode: REASON.timedOut, error: `stdin was idle for ${idleTimeoutMs} ms before the bundle was complete` }, true), idleTimeoutMs);
    };
    const onData = (chunk) => {
      size += chunk.length;
      if (size > MAX_ARCHIVE_BYTES) {
        finish({ ok: false, reasonCode: REASON.badArgument, error: `the bundle on stdin is larger than ${MAX_ARCHIVE_BYTES} bytes` }, true);
        return;
      }
      chunks.push(Buffer.from(chunk));
      armIdle();
    };
    const onEnd = () => finish({ ok: true, bytes: Buffer.concat(chunks), source: 'stdin' });
    const onError = (error) => finish({ ok: false, reasonCode: REASON.badArgument, error: `stdin could not be read: ${error.message}` }, true);
    input.on('data', onData);
    input.once('end', onEnd);
    input.once('error', onError);
    armIdle();
    totalTimer = setTimeout(() => finish({ ok: false, reasonCode: REASON.timedOut, error: `stdin did not finish within ${totalTimeoutMs} ms` }, true), totalTimeoutMs);
    input.resume?.();
  });
}

function liveState(base = basePath()) {
  const version = treeVersion(agentDir(base)).version;
  return { version, contract: version === AGENT_VERSION ? CONTRACT_VERSION : null };
}

function previousState(base = basePath()) {
  const previous = treeVersion(previousDir(base));
  return previous.version ? { version: previous.version, path: previous.path } : null;
}

function failure(message, reasonCode = REASON.internal, extra = {}) {
  return { ok: false, action: 'failed', reasonCode, message, ...extra };
}

export async function checkBundle(source) {
  const read = await readBundle(source);
  if (!read.ok) return failure(read.error, read.reasonCode, { manifest: null });
  const verified = verifyAgentArchive(read.bytes);
  if (!verified.ok) return failure(`${verified.error}; nothing was extracted and the running agent is untouched`, REASON.signatureInvalid, { manifest: null });
  return {
    ok: true,
    action: 'checked',
    reasonCode: null,
    message: `the bundle carries agent ${verified.manifest.version} (contract ${verified.manifest.contract}) and its signature matches release key ${trustFingerprint().slice(0, 8)}`,
    manifest: manifestShape(verified),
  };
}

function replayTarget(opId, variant) {
  if (!opId) return null;
  const existing = readOperation(opId);
  if (!existing) return null;
  if (existing.kind !== 'self-update') return variant;
  return typeof existing.target === 'string' && existing.target.startsWith(`${variant}:`) ? existing.target : variant;
}

/** Publish only authenticated recovery helpers, before the live tree can move. */
function publishStableLauncher(files, base) {
  const windows = process.platform === 'win32';
  const wrapper = windows ? 'install/launcher.ps1' : 'install/launcher.sh';
  const dispatcher = windows ? 'install/dispatch.ps1' : 'install/dispatch.sh';
  for (const name of ['install/launcher.mjs', wrapper, dispatcher]) {
    if (!Buffer.isBuffer(files.get(name))) throw new Error(`the signed bundle is missing the recovery helper ${name}`);
  }
  const nodeLiteral = windows ? `'${process.execPath.replaceAll("'", "''")}'` : `'${process.execPath.replaceAll("'", "'\\''")}'`;
  const marker = windows ? '@NODE_POWERSHELL@' : '@NODE_SHELL@';
  const wrapperText = files.get(wrapper).toString('utf8');
  if (!wrapperText.includes(marker)) throw new Error(`the signed recovery wrapper has no ${marker} placeholder`);
  const outputs = [
    [windows ? 'legionctl.ps1' : 'legionctl', Buffer.from(wrapperText.replaceAll(marker, nodeLiteral)), windows ? 0o644 : 0o755],
    [windows ? 'dispatch.ps1' : 'dispatch.sh', files.get(dispatcher), windows ? 0o644 : 0o755],
    ['launcher.mjs', files.get('install/launcher.mjs'), 0o644],
  ];
  const directory = path.join(base, 'bin');
  fs.mkdirSync(directory, { recursive: true, mode: 0o700 });
  for (const [name, bytes, mode] of outputs) {
    const destination = path.join(directory, name);
    const temporary = `${destination}.${process.pid}.tmp`;
    try {
      fs.writeFileSync(temporary, bytes, { mode });
      fs.renameSync(temporary, destination);
    } finally { try { fs.unlinkSync(temporary); } catch { /* renamed or preserved on failure */ } }
  }
}

async function installVerified(config, verified, { target, base = basePath(), ...operation } = {}) {
  const manifest = manifestShape(verified);
  return withOperation(
    { kind: 'self-update', target, systemId: config?.system?.id ?? null, ...operation },
    async ({ opId, lock }) => {
      // A replay returns from withOperation before this body, so verified may be
      // null only on that path.
      const recovered = recoverAgentSwap(base);
      if (!recovered.ok) return failure(recovered.error, REASON.internal, { current: liveState(base), previous: previousState(base), manifest, selfTest: null });
      lock.phase('staging');
      recordPhase(opId, 'staging', { step: 2, of: 4, note: `agent ${manifest.version}` });
      removeTree(stagingDir(base));
      const staged = stageFiles(verified.files, stagingDir(base));
      if (!staged.ok) return failure(`the verified agent could not be staged: ${staged.error}`, REASON.internal, { current: liveState(base), previous: previousState(base), manifest, selfTest: null });

      lock.phase('self-test');
      recordPhase(opId, 'self-test', { step: 3, of: 4, note: 'running the staged agent' });
      const test = selfTest(stagingDir(base), { expectedVersion: manifest.version, expectedContract: manifest.contract });
      if (!test.ok) {
        removeTree(stagingDir(base));
        return failure(`the staged agent ${manifest.version} failed its own self-test, so it was discarded and the live agent is untouched: ${test.output}`, REASON.internal, {
          current: liveState(base), staged: null, previous: previousState(base), manifest, selfTest: test,
        });
      }

      lock.phase('swapping');
      recordPhase(opId, 'swapping', { step: 4, of: 4, note: 'renaming the trees' });
      try { publishStableLauncher(verified.files, base); }
      catch (error) {
        removeTree(stagingDir(base));
        return failure(`the recovery launcher could not be installed, so the live agent is untouched: ${error.message}`, REASON.internal, {
          current: liveState(base), staged: null, previous: previousState(base), manifest, selfTest: test,
        });
      }
      const before = liveState(base);
      note(`installing agent ${manifest.version} over ${before.version ?? 'an earlier agent'}`, 'self-update');
      const swapped = commitInstallSwap(base);
      if (!swapped.ok) return failure(swapped.error, REASON.internal, { current: liveState(base), previous: previousState(base), manifest, selfTest: test, notes: swapped.notes });
      const message = `agent ${manifest.version} is installed${before.version ? ` and ${before.version} is kept at ${previousDir(base)}` : ''}`;
      log(message, 'self-update');
      return {
        ok: true, action: 'installed', reasonCode: null, message, from: before.version, to: manifest.version,
        current: { version: manifest.version, contract: manifest.contract ?? CONTRACT_VERSION }, staged: null,
        previous: before.version ? { version: before.version, path: previousDir(base) } : null,
        manifest, selfTest: test, ...(swapped.notes.length ? { notes: swapped.notes } : {}),
      };
    },
  );
}

/** Acquire and authenticate the upload before opening an operation or taking its lock. */
export async function runSelfUpdate(config, options = {}) {
  const read = await readBundle(options);
  if (!read.ok) return failure(read.error, read.reasonCode, { current: liveState(), staged: null, previous: previousState(), manifest: null, selfTest: null });
  const target = `install:${digest(read.bytes)}`;
  const verified = verifyAgentArchive(read.bytes);
  if (!verified.ok) {
    log(`self-update refused: ${verified.error}`, 'self-update');
    return failure(`${verified.error}; nothing was extracted and the running agent is untouched`, REASON.signatureInvalid, {
      current: liveState(), staged: null, previous: previousState(), manifest: null, selfTest: null,
    });
  }
  return installVerified(config, verified, { target, ...options });
}

/** Resolve <base>/incoming/agent/src/index.mjs without consulting the cwd. */
export function resolveBootstrapLayout(entryUrl = import.meta.url, configuredBase = process.env.LEGIONCTL_HOME ?? null) {
  const entry = path.resolve(fileURLToPath(entryUrl));
  const source = path.resolve(path.dirname(entry), '..');
  if (path.basename(source) !== 'agent' || path.basename(path.dirname(source)) !== 'incoming') {
    return { ok: false, error: '--install must run from <LEGIONCTL_HOME>/incoming/agent/src/index.mjs' };
  }
  const base = path.dirname(path.dirname(source));
  if (configuredBase && path.resolve(configuredBase) !== base) {
    return { ok: false, error: `LEGIONCTL_HOME resolves to ${path.resolve(configuredBase)}, but the staged agent belongs to ${base}` };
  }
  return { ok: true, base, source, entry };
}

/** Bootstrap over 2.x from the already extracted, signed incoming tree. */
export async function runBootstrapInstall(config, options = {}) {
  const layout = options.layout ?? resolveBootstrapLayout(options.entryUrl, options.configuredBase);
  if (!layout.ok) return failure(layout.error, REASON.badArgument, { current: liveState(), staged: null, previous: previousState(), manifest: null, selfTest: null });
  const verified = verifyAgentTree(layout.source);
  if (!verified.ok) return failure(`the staged agent did not verify: ${verified.error}; the installed agent is untouched`, REASON.signatureInvalid, {
    current: liveState(layout.base), staged: null, previous: previousState(layout.base), manifest: null, selfTest: null,
  });
  const target = `install:${digest(verified.files.get('MANIFEST.json'))}`;
  return installVerified(config, verified, { target, base: layout.base, ...options });
}

/** Roll back only to a complete tree authenticated by the pinned release key. */
export async function runRollback(config, options = {}) {
  const replay = replayTarget(options.opId, 'rollback');
  if (replay?.startsWith('rollback:')) {
    return withOperation({ kind: 'self-update', target: replay, systemId: config?.system?.id ?? null, ...options }, async () => {
      throw new Error('a replayed rollback body must not execute');
    });
  }
  if (replay === 'rollback') {
    return withOperation({ kind: 'self-update', target: 'rollback', systemId: config?.system?.id ?? null, ...options }, async () => {
      throw new Error('a conflicting rollback body must not execute');
    });
  }

  const base = basePath();
  const verified = verifyAgentTree(previousDir(base));
  if (!verified.ok) {
    const retained = fs.existsSync(previousDir(base));
    return failure(
      retained
        ? `the previous agent is retained at ${previousDir(base)}, but it is not a complete tree signed by the release key (${verified.error}); restore it manually after independent verification`
        : `there is no previous agent at ${previousDir(base)} to roll back to`,
      retained ? REASON.signatureInvalid : REASON.notConfigured,
      { current: liveState(base), staged: null, previous: previousState(base), manifest: null, selfTest: null },
    );
  }
  const target = `rollback:${digest(verified.files.get('MANIFEST.json'))}`;
  const manifest = manifestShape(verified);
  return withOperation(
    { kind: 'self-update', target, systemId: config?.system?.id ?? null, ...options },
    async ({ opId, lock }) => {
      const recovered = recoverAgentSwap(base);
      if (!recovered.ok) return failure(recovered.error, REASON.internal, { current: liveState(base), previous: previousState(base), manifest: null, selfTest: null });
      const rollbackStage = swapPaths(base).rollbackStaging;
      removeTree(rollbackStage);
      const staged = stageFiles(verified.files, rollbackStage);
      if (!staged.ok) return failure(`the verified previous agent could not be prepared: ${staged.error}`, REASON.internal, { current: liveState(base), previous: previousState(base), manifest, selfTest: null });
      lock.phase('self-test');
      recordPhase(opId, 'self-test', { note: 'running the verified rollback copy' });
      const test = selfTest(rollbackStage, { expectedVersion: manifest.version, expectedContract: manifest.contract });
      if (!test.ok) {
        removeTree(rollbackStage);
        return failure(`the verified previous agent ${manifest.version} does not start, so it was left inactive: ${test.output}`, REASON.internal, {
          current: liveState(base), staged: null, previous: previousState(base), manifest, selfTest: test,
        });
      }
      lock.phase('swapping');
      recordPhase(opId, 'swapping');
      const before = liveState(base);
      const swapped = commitRollbackSwap(base);
      if (!swapped.ok) return failure(swapped.error, REASON.internal, { current: liveState(base), previous: previousState(base), manifest, selfTest: test, notes: swapped.notes });
      const retainedPath = fs.existsSync(swapPaths(base).rollbackParked) ? swapPaths(base).rollbackParked : previousDir(base);
      const message = `rolled back to agent ${manifest.version}; ${before.version ?? 'the outgoing agent'} is kept at ${retainedPath}`;
      log(message, 'self-update');
      return {
        ok: true, action: 'rolled-back', reasonCode: null, message, from: before.version, to: manifest.version,
        current: { version: manifest.version, contract: manifest.contract ?? null }, staged: null,
        previous: { version: before.version, path: retainedPath }, manifest, selfTest: test,
        ...(swapped.notes.length ? { notes: swapped.notes } : {}),
      };
    },
  );
}

export function ensureIncoming() {
  const dir = incomingDir();
  fs.mkdirSync(dir, { recursive: true });
  return dir;
}
