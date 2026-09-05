// Boot and sleep as operations.
//
// These are the two verbs whose outcome the agent can never observe, because the
// machine goes down underneath the process that was asked. Everything here is
// arranged around that one fact:
//
//   The reply says the request was ACCEPTED, never that it happened. The record
//   stays open with the boot identity of the machine as it was. A bounded
//   dispatch grace blocks further mutations after the command process exits.
//   When the machine comes back, the next mutating command or cycle finds a
//   record whose pid is gone and judges it — and it only reads as `rebooted`
//   when the boot identity really changed AND the system now running is the one
//   that was asked for. A shorter uptime on its own proves nothing: the machine
//   could have been power cycled, or rebooted by something else, or come back
//   into the system it was already in because the arming silently failed.
//
//   A suspend leaves no trace at all once the machine is awake again, so an
//   interrupted sleep is reported as unknown rather than guessed at.
//
// Arming is always read back before anything reboots. A reboot that was not
// verifiably armed just returns to the system we are already on, and reporting
// that as success sends the controller off waiting for a machine that is never
// going to appear.

import { armNextBoot, bootTargets, findBootTarget, scheduleReboot, suspendMachine } from './boot.mjs';
import { REASON } from './contract.mjs';
import { log } from './log.mjs';
import { withOperation } from './operate.mjs';
import { markPowerDispatch, recordPhase } from './operations.mjs';
import { busyReasonCode, checkAllBusy } from './probes/busy.mjs';
import { livenessOf } from './service.mjs';

async function machineBusy(config) {
  const liveness = {};
  for (const service of config.services) liveness[service.id] = livenessOf(service);
  const { busy } = await checkAllBusy(config, { liveness });
  return busy;
}

/**
 * Arm a boot target and reboot into it.
 *
 * `--no-reboot` stops after the arming, which is genuinely useful: it lets a
 * person confirm the firmware took the change before committing to losing the
 * session.
 */
export async function runBoot(config, target, options = {}) {
  const { force = false, noReboot = false } = options;

  return withOperation(
    { kind: 'boot', target, systemId: config.system.id, ...options },
    async ({ opId, lock }) => {
      if (target && target === config.system.id) {
        return {
          ok: true,
          action: 'noop',
          reasonCode: REASON.alreadyOnTarget,
          target,
          armed: false,
          message: `already running ${config.system.name}`,
        };
      }

      const configured = bootTargets(config);
      if (configured.length === 0) {
        // A single-system machine has nothing to point at, and reaching efibootmgr
        // or bcdedit here would fail with something unreadable instead.
        return {
          ok: false,
          action: 'failed',
          reasonCode: REASON.notConfigured,
          target: target ?? null,
          armed: false,
          message: `${config.system.name} has no boot targets configured, so there is no other system to point it at`,
        };
      }

      if (!target || !findBootTarget(config, target)) {
        return {
          ok: false,
          action: 'failed',
          reasonCode: REASON.unknownTarget,
          target: target ?? null,
          armed: false,
          message: `boot takes one target: ${configured.map((entry) => entry.id).join(' or ')}`,
        };
      }

      lock.phase('checking-busy');
      recordPhase(opId, 'checking-busy');
      const busy = await machineBusy(config);
      if (busy.busy && !force) {
        log(`boot to ${target} deferred: ${busy.reason}`, 'boot');
        return {
          ok: true,
          action: 'deferred',
          reasonCode: busyReasonCode(busy),
          target,
          armed: false,
          busy,
          message: `the machine is busy (${busy.reason}); pass --force to reboot anyway, or --when-idle to queue it`,
        };
      }

      lock.phase('arming');
      recordPhase(opId, 'arming');
      // Verification is required whenever a reboot follows. A command target
      // with no verify command is refused here, and --force does not lift it.
      const armed = armNextBoot(config, target, { requireVerification: !noReboot });
      if (!armed.ok) {
        log(`boot to ${target} failed: ${armed.message}`, 'boot');
        return {
          ok: false,
          action: 'failed',
          reasonCode: REASON.notConfigured,
          target,
          armed: false,
          message: `${armed.message}; the machine was left running ${config.system.name} and was not rebooted`,
        };
      }

      if (noReboot) {
        log(`boot to ${target} armed without rebooting: ${armed.message}`, 'boot');
        return { ok: true, action: 'armed', reasonCode: null, target, armed: true, message: armed.message };
      }

      lock.phase('rebooting');
      markPowerDispatch(opId, 'rebooting');
      const reboot = scheduleReboot(config);
      if (!reboot.ok) {
        log(`boot to ${target} armed but the reboot failed: ${reboot.message}`, 'boot');
        return {
          ok: false,
          action: 'failed',
          reasonCode: REASON.notConfigured,
          target,
          armed: true,
          message: `armed (${armed.message}) but the reboot could not be scheduled: ${reboot.message}`,
        };
      }

      log(`boot to ${target}: ${armed.message}; ${reboot.message}`, 'boot');
      // "rebooting", not "rebooted". Whether it happened is settled later, by
      // recovery, from the boot identity and the system that comes back.
      return {
        ok: true,
        action: 'rebooting',
        pendingTransition: true,
        reasonCode: null,
        target,
        armed: true,
        message: `${armed.message}; ${reboot.message}. Ask "op ${opId}" once the machine is back to find out whether it came up in ${target}`,
      };
    },
  );
}

/**
 * Suspend the machine this agent is running on.
 *
 * The busy gate is the same one boot and restart use, and for the same reason: a
 * machine that is asleep is a machine that dropped whatever it was in the middle
 * of. Sleeping is cheap to defer and expensive to get wrong, so without --force a
 * busy machine wins.
 *
 * The success wording is careful on purpose. All we ever learn is that the
 * suspend command was accepted, since the machine goes down underneath us and
 * there is nothing left running to confirm it. Reporting "it slept" on the
 * strength of an exit code would be the exact lie this command must not tell.
 */
export async function runSleep(config, options = {}) {
  const { force = false } = options;

  return withOperation({ kind: 'sleep', systemId: config.system.id, ...options }, async ({ opId, lock }) => {
    lock.phase('checking-busy');
    recordPhase(opId, 'checking-busy');
    const busy = await machineBusy(config);
    if (busy.busy && !force) {
      log(`sleep deferred: ${busy.reason}`, 'sleep');
      return {
        ok: true,
        action: 'deferred',
        reasonCode: busyReasonCode(busy),
        busy,
        message: `the machine is busy (${busy.reason}); pass --force to sleep anyway, or --when-idle to queue it`,
      };
    }

    lock.phase('suspending');
    markPowerDispatch(opId, 'suspending');
    const suspended = await suspendMachine(config);
    if (!suspended.ok) {
      log(`sleep failed: ${suspended.message}`, 'sleep');
      return { ok: false, action: 'failed', reasonCode: REASON.notConfigured, message: suspended.message };
    }

    lock.phase('suspending');
    recordPhase(opId, 'suspending');
    log(`sleep: ${suspended.message}`, 'sleep');
    return { ok: true, action: 'sleeping', pendingTransition: true, reasonCode: null, message: suspended.message };
  });
}
