// Configured actions: a named command you want a button for.
//
// An action has no version, no health and no busy state of its own. It is just
// something to run, and the only judgement the agent makes about one is whether
// the machine is busy and the action said it cares.

import { runArgv } from './config.mjs';
import { checkAllBusy } from './probes/busy.mjs';
import { log, note } from './log.mjs';

const MAX_OUTPUT_LINES = 20;

export function findAction(config, id) {
  return config.actions.find((action) => action.id === id) ?? null;
}

/** What the apps show under `actions` in status: everything except the command. */
export function listActions(config) {
  return config.actions.map((action) => ({
    id: action.id,
    name: action.name,
    confirm: action.confirm ?? null,
    busyGated: action.busyGated === true,
  }));
}

/**
 * The first lines of what the command said, both streams together. Long output
 * belongs in the machine's own logs, not in a JSON reply that has to cross an
 * ssh session and land in a phone.
 */
export function shapeOutput(stdout, stderr, maxLines = MAX_OUTPUT_LINES) {
  const lines = `${stdout ?? ''}\n${stderr ?? ''}`
    .split('\n')
    .map((line) => line.trimEnd())
    .filter((line) => line.trim().length > 0);
  if (lines.length === 0) return '';
  const shown = lines.slice(0, maxLines);
  const hidden = lines.length - shown.length;
  return hidden > 0 ? `${shown.join('\n')}\n… ${hidden} more line${hidden === 1 ? '' : 's'}` : shown.join('\n');
}

/** Run one configured action. Always returns an object; never throws. */
export async function runAction(config, id, { force = false } = {}) {
  const action = findAction(config, id);
  if (!action) {
    const known = config.actions.map((entry) => entry.id).join(', ') || 'none are configured';
    return {
      ok: false,
      action: 'failed',
      id: id ?? null,
      exitCode: null,
      output: '',
      message: `unknown action: ${id ?? '(none given)'}; this system offers ${known}`,
    };
  }

  if (action.busyGated && !force) {
    const { busy } = await checkAllBusy(config);
    if (busy.busy) {
      log(`action ${action.id} deferred: ${busy.reason}`, 'run');
      return {
        ok: true,
        action: 'deferred',
        id: action.id,
        exitCode: null,
        output: '',
        message: `the machine is busy (${busy.reason}); pass --force to run ${action.name} anyway`,
        busy,
      };
    }
  }

  note(`running ${action.name}${force ? ' (forced)' : ''}`, 'run');
  const result = runArgv(action.command, { timeoutMs: action.timeoutSeconds * 1000 });
  const output = shapeOutput(result.stdout, result.stderr);

  if (result.timedOut) {
    const message = `${action.name} did not finish within ${action.timeoutSeconds} s and was stopped`;
    log(message, 'run');
    return { ok: false, action: 'failed', id: action.id, exitCode: null, output, message };
  }
  if (!result.ok) {
    const message = `${action.name} exited ${result.code ?? 'without a code'}`;
    log(message, 'run');
    return { ok: false, action: 'failed', id: action.id, exitCode: result.code ?? null, output, message };
  }

  log(`${action.name} ran`, 'run');
  return { ok: true, action: 'ran', id: action.id, exitCode: 0, output, message: `${action.name} ran` };
}
