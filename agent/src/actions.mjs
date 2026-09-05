// Configured actions: a named command you want a button for.
//
// An action has no version, no health and no busy state of its own. It is just
// something to run, and the only judgement the agent makes about one is whether
// the machine is busy and the action said it cares.
//
// It does go through the same machine lock as everything else, though, and that
// is not ceremony. An action is an arbitrary command somebody wrote to do
// something to this machine; running it in the middle of an install is exactly
// as bad as any other collision, and there is no way from here to know which
// ones are safe.

import dgram from 'node:dgram';
import { runArgv } from './config.mjs';
import { REASON } from './contract.mjs';
import { withOperation } from './operate.mjs';
import { recordPhase } from './operations.mjs';
import { busyReasonCode, checkAllBusy } from './probes/busy.mjs';
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
    // What running it does. A client renders a wake action differently from a
    // command, and must never show the argv of either.
    kind: action.kind ?? 'command',
  }));
}

/** The 102 byte wake-on-LAN magic packet: six 0xFF bytes, then the MAC 16 times. */
export function magicPacket(mac) {
  if (typeof mac !== 'string' || !/^[0-9a-f]{2}(?:[:-][0-9a-f]{2}){5}$/i.test(mac)) throw new Error('the MAC must contain six hex pairs');
  const bytes = Buffer.from(mac.split(/[:-]/).map((pair) => Number.parseInt(pair, 16)));
  if (bytes.length !== 6) throw new Error(`${mac} is not six hex pairs`);
  return Buffer.concat([Buffer.alloc(6, 0xff), ...new Array(16).fill(bytes)]);
}

/**
 * Send the magic packet from this machine.
 *
 * A wake packet is a link-layer broadcast, so it only ever reaches machines on
 * the sender's own network. That is the entire point: a machine that is already
 * awake on the sleeping machine's LAN can wake it, whether or not the person
 * pressing the button is anywhere near either of them. It is why a wake helper
 * has to BE on that network and why one helper cannot serve two sites.
 *
 * Every failure is reported per address rather than as one aggregate: "could not
 * wake it" is useless next to "the broadcast address is not on any interface".
 */
export function sendMagicPacket(wol, { timeoutMs = 5000 } = {}) {
  return new Promise((resolve) => {
    let packet;
    try {
      packet = magicPacket(wol.mac);
    } catch (err) {
      resolve({ sent: 0, attempted: 0, errors: [err.message], targets: [] });
      return;
    }

    const socket = dgram.createSocket({ type: 'udp4', reuseAddr: true });
    const targets = [];
    for (const address of wol.broadcast) for (const port of wol.ports) targets.push({ address, port });

    const errors = [];
    let sent = 0;
    let outstanding = targets.length * wol.repeats;
    let settled = false;
    const finish = () => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      try {
        socket.close();
      } catch {
        /* already closed */
      }
      resolve({ sent, attempted: targets.length * wol.repeats, errors, targets });
    };
    const timer = setTimeout(() => {
      errors.push(`sending timed out after ${timeoutMs} ms`);
      finish();
    }, timeoutMs);

    socket.on('error', (err) => {
      errors.push(err.message);
      finish();
    });

    socket.bind(() => {
      try {
        socket.setBroadcast(true);
      } catch (err) {
        errors.push(`this machine will not send broadcasts: ${err.message}`);
        finish();
        return;
      }
      if (outstanding === 0) {
        finish();
        return;
      }
      for (const target of targets) {
        for (let repeat = 0; repeat < wol.repeats; repeat += 1) {
          socket.send(packet, target.port, target.address, (err) => {
            if (err) {
              const detail = `${target.address}:${target.port}: ${err.message}`;
              if (!errors.includes(detail)) errors.push(detail);
            } else {
              sent += 1;
            }
            outstanding -= 1;
            if (outstanding === 0) finish();
          });
        }
      }
    });
  });
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
export async function runAction(config, id, options = {}) {
  const { force = false } = options;
  const action = findAction(config, id);
  if (!action) {
    const known = config.actions.map((entry) => entry.id).join(', ') || 'none are configured';
    return {
      ok: false,
      action: 'failed',
      reasonCode: REASON.unknownAction,
      id: id ?? null,
      exitCode: null,
      output: '',
      message: `unknown action: ${id ?? '(none given)'}; this system offers ${known}`,
    };
  }

  const result = await withOperation(
    { kind: 'run', actionId: action.id, systemId: config.system.id, ...options },
    async ({ opId, lock }) => {
      if (action.busyGated && !force) {
        lock.phase('checking-busy');
        recordPhase(opId, 'checking-busy');
        const { busy } = await checkAllBusy(config);
        if (busy.busy) {
          log(`action ${action.id} deferred: ${busy.reason}`, 'run');
          return {
            ok: true,
            action: 'deferred',
            reasonCode: busyReasonCode(busy),
            id: action.id,
            exitCode: null,
            output: '',
            message: `the machine is busy (${busy.reason}); pass --force to run ${action.name} anyway, or --when-idle to queue it`,
            busy,
          };
        }
      }

      lock.phase('running');
      recordPhase(opId, 'running', { note: action.id });
      note(`running ${action.name}${force ? ' (forced)' : ''}`, 'run');

      if (action.kind === 'wol') {
        const result = await sendMagicPacket(action.wol, { timeoutMs: action.timeoutSeconds * 1000 });
        const where = result.targets.map((target) => `${target.address}:${target.port}`).join(', ');
        if (result.sent === 0) {
          const message =
            `no wake packet reached the wire for ${action.wol.mac}` +
            (result.errors.length > 0 ? `: ${result.errors.join('; ')}` : '');
          log(message, 'run');
          return { ok: false, action: 'failed', reasonCode: REASON.internal, id: action.id, exitCode: null, output: '', message };
        }
        // Sending is not waking. The packet leaving this machine is all that can
        // be observed from here; whether the target came up is answered by the
        // controller's own readiness poll, not by this reply.
        const message = `sent ${result.sent} wake packet${result.sent === 1 ? '' : 's'} for ${action.wol.mac} to ${where}`;
        const output = [message, ...result.errors.map((line) => `warning: ${line}`)].join('\n');
        log(message, 'run');
        return { ok: true, action: 'ran', reasonCode: null, id: action.id, exitCode: 0, output, message };
      }

      const run = runArgv(action.command, { timeoutMs: action.timeoutSeconds * 1000 });
      const output = shapeOutput(run.stdout, run.stderr);

      if (run.timedOut) {
        const message = `${action.name} did not finish within ${action.timeoutSeconds} s and was stopped`;
        log(message, 'run');
        return {
          ok: false,
          action: 'failed',
          reasonCode: REASON.timedOut,
          id: action.id,
          exitCode: null,
          output,
          message,
        };
      }
      if (!run.ok) {
        const message = `${action.name} exited ${run.code ?? 'without a code'}`;
        log(message, 'run');
        return {
          ok: false,
          action: 'failed',
          reasonCode: REASON.internal,
          id: action.id,
          exitCode: run.code ?? null,
          output,
          message,
        };
      }

      log(`${action.name} ran`, 'run');
      return {
        ok: true,
        action: 'ran',
        reasonCode: null,
        id: action.id,
        exitCode: 0,
        output,
        message: `${action.name} ran`,
      };
    },
  );
  return { id: action.id, ...result };
}
