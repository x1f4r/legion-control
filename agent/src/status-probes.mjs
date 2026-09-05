import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { describeFailure, runCommandAsync } from './config.mjs';
import { checkServiceBusy, unreadable } from './probes/busy.mjs';

const worker = fileURLToPath(new URL('./status-probe-worker.mjs', import.meta.url));
let knownBootIdentity = null;

export async function statusBootIdentity(clock) {
  if (knownBootIdentity) return knownBootIdentity;
  const timeoutMs = clock.slice(10000);
  if (timeoutMs <= 0) return null;
  const result = await runCommandAsync(process.execPath, [worker, 'boot-identity'], { timeoutMs });
  if (!result.ok) return null;
  try {
    const value = JSON.parse(result.stdout);
    if (typeof value === 'string' && value.length > 0) knownBootIdentity = value;
  } catch { /* An unavailable boot identity is never successful reboot evidence. */ }
  return knownBootIdentity;
}

export async function statusBusy(service, { clock, liveness }) {
  const timeoutMs = clock.slice((service.busy?.timeoutSeconds ?? 8) * 1000);
  if (service.busy?.type !== 't3-sqlite') return checkServiceBusy(service, { timeoutMs, liveness });
  if (timeoutMs <= 0) return unreadable('timed-out', 'there was no time left in the status budget for the busy database');
  // A SQLite WAL fallback writes only a temporary copy. Keep every copy under
  // one parent-owned directory, which can be removed even if the worker is killed.
  const scratch = fs.mkdtempSync(path.join(os.tmpdir(), 'legionctl-status-probe-'));
  let exited = false;
  let deferCleanup = false;
  let cleanupAttempted = false;
  let cleanupError = null;
  const cleanup = () => {
    cleanupAttempted = true;
    try { fs.rmSync(scratch, { recursive: true, force: true }); cleanupError = null; }
    catch (error) { cleanupError = `temporary busy-probe files could not be removed: ${error.message}`; }
  };
  try {
    const result = await runCommandAsync(process.execPath,
      [worker, 'sqlite-busy', JSON.stringify({ service: { name: service.name, busy: service.busy }, liveness })],
      // This worker only opens SQLite and reads files; it never spawns a child.
      // Confirm its exit before removing scratch or releasing source handles.
      { timeoutMs, terminateTree: false, onExit: () => { exited = true; cleanup(); },
        env: { ...process.env, TMPDIR: scratch, TMP: scratch, TEMP: scratch }, maxBuffer: 1024 * 1024 });
    if (process.platform === 'win32' && result.timedOut && result.terminationConfirmed !== true && !exited) {
      // Failed termination is unknown, not a reason to delete files a live
      // worker may still own. Its eventual exit callback completes cleanup.
      deferCleanup = true;
      return unreadable('timed-out', `${describeFailure(result)}; worker exit is unconfirmed, so temporary files are retained until it exits`);
    }
    if (!cleanupAttempted) cleanup();
    if (cleanupError) return unreadable(result.timedOut ? 'timed-out' : 'probe-error', cleanupError);
    if (!result.ok) return unreadable(result.timedOut ? 'timed-out' : 'probe-error', describeFailure(result));
    try {
      const value = JSON.parse(result.stdout);
      if (typeof value.busy === 'boolean' && typeof value.unknown === 'boolean') return value;
    } catch { /* A malformed or cut-off answer must retain protection. */ }
    return unreadable('probe-error', 'the busy database probe returned an unreadable answer');
  } finally {
    if (!deferCleanup && !cleanupAttempted) cleanup();
  }
}
