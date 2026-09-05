// The machine's power state: which system it comes back up in, and taking it
// down (reboot, suspend).
//
// Arming a boot target is always read back and verified before anything
// reboots. A reboot that was not verifiably armed is refused, because rebooting
// an unarmed machine just returns to the system it was already on, and the
// controller apps would then sit waiting for one that never comes.

import { spawn } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { describeFailure, detectPlatform, runArgv, runCommand, sleep } from './config.mjs';

// ---------------------------------------------------------------------------
// efi-bootnext
// ---------------------------------------------------------------------------

/** Strip the device path that efibootmgr appends after the description. */
export function entryDescription(rest) {
  const beforeTab = rest.split('\t')[0];
  const cut = beforeTab.search(/\s+(HD\(|PciRoot\(|PcieRoot\(|VenHw\(|VenMedia\(|Fv\(|BBS\(|UsbClass\()/);
  return (cut === -1 ? beforeTab : beforeTab.slice(0, cut)).trim();
}

/** Turn `efibootmgr` output into its entries and the currently armed BootNext. */
export function parseEfiBootManager(stdout) {
  const entries = [];
  let bootNext = null;
  for (const line of String(stdout ?? '').split('\n')) {
    const next = line.match(/^BootNext:\s*([0-9A-Fa-f]{4})/);
    if (next) {
      bootNext = next[1].toUpperCase();
      continue;
    }
    const entry = line.match(/^Boot([0-9A-Fa-f]{4})\*?\s+(.*)$/);
    if (entry) entries.push({ number: entry[1].toUpperCase(), description: entryDescription(entry[2]) });
  }
  return { entries, bootNext };
}

function readEfiBootManager() {
  let result = runCommand('efibootmgr', [], { timeoutMs: 15000 });
  if (!result.ok) {
    // Some setups only expose the EFI variables to root.
    result = runCommand('sudo', ['-n', 'efibootmgr'], { timeoutMs: 15000 });
  }
  if (!result.ok) return { ok: false, message: describeFailure(result), entries: [], bootNext: null };
  return { ok: true, message: null, ...parseEfiBootManager(result.stdout) };
}

/**
 * Pick the entry a target's `match` names. Pure, so the matching rule can be
 * tested against real efibootmgr output without a machine that has firmware.
 *
 * Only an entry that really matches may be armed. A pattern that merely contains
 * the word would happily pick a recovery entry or an installer stick, and a
 * BootNext into either of those comes up in a menu with no network and no ssh,
 * which means a trip to the machine itself. Anchor the patterns.
 */
export function matchEfiEntry(entries, matchSource) {
  let pattern;
  try {
    pattern = new RegExp(matchSource, 'i');
  } catch {
    return { ok: false, message: `the boot target's match is not a valid regular expression: ${matchSource}` };
  }
  const candidates = entries.filter((entry) => pattern.test(entry.description));
  if (candidates.length === 0) {
    return {
      ok: false,
      message: `no EFI entry matching ${JSON.stringify(matchSource)} (found: ${
        entries.map((entry) => entry.description).join(', ') || 'nothing'
      })`,
    };
  }
  return { ok: true, entry: candidates[0], ambiguous: candidates.length > 1, message: null };
}

/** Resolve the entry fresh, every single time. Firmware renumbers Boot#### entries. */
export function resolveEfiEntry(matchSource) {
  const listing = readEfiBootManager();
  if (!listing.ok) return { ok: false, message: `could not read the EFI boot entries: ${listing.message}` };
  return matchEfiEntry(listing.entries, matchSource);
}

function armEfiBootNext(target) {
  const resolved = resolveEfiEntry(target.match);
  if (!resolved.ok) return { ok: false, message: resolved.message };

  const number = resolved.entry.number;
  const set = runCommand('sudo', ['-n', 'efibootmgr', '--bootnext', number], { timeoutMs: 20000 });
  if (!set.ok) return { ok: false, message: `could not set BootNext=${number}: ${describeFailure(set)}` };

  const verify = readEfiBootManager();
  if (!verify.ok || verify.bootNext !== number) {
    return {
      ok: false,
      message: `BootNext did not stick (wanted ${number}, firmware reports ${verify.bootNext ?? 'nothing'})`,
    };
  }
  return {
    ok: true,
    message: `BootNext=${number} (${resolved.entry.description})${
      resolved.ambiguous ? ', several entries matched' : ''
    }`,
  };
}

// ---------------------------------------------------------------------------
// bcdedit
// ---------------------------------------------------------------------------

function readBootSequence() {
  const enumerated = runCommand('bcdedit', ['/enum', '{fwbootmgr}'], { timeoutMs: 20000 });
  if (!enumerated.ok) return { ok: false, value: null, message: describeFailure(enumerated) };
  const line = enumerated.stdout.split('\n').find((row) => /^\s*bootsequence\b/i.test(row));
  return { ok: true, value: line ? line.replace(/^\s*bootsequence\s*/i, '').trim() : null, message: null };
}

function armClearBootSequence() {
  const before = readBootSequence();
  if (!before.ok) return { ok: false, message: `could not read {fwbootmgr}: ${before.message}` };
  if (!before.value) {
    return { ok: true, message: 'bootsequence was already clear; the next boot falls through BootOrder' };
  }

  const cleared = runCommand('bcdedit', ['/deletevalue', '{fwbootmgr}', 'bootsequence'], { timeoutMs: 20000 });
  if (!cleared.ok) return { ok: false, message: `could not clear bootsequence: ${describeFailure(cleared)}` };

  const after = readBootSequence();
  if (after.ok && after.value) return { ok: false, message: 'bootsequence is still set after the delete' };
  return { ok: true, message: 'cleared {fwbootmgr} bootsequence; the next boot falls through BootOrder' };
}

function armBootSequence(target) {
  const set = runCommand('bcdedit', ['/set', '{fwbootmgr}', 'bootsequence', target.entry], { timeoutMs: 20000 });
  if (!set.ok) return { ok: false, message: `could not set bootsequence: ${describeFailure(set)}` };

  const after = readBootSequence();
  if (!after.ok) return { ok: false, message: `bootsequence was set but could not be read back: ${after.message}` };
  if (!after.value || !after.value.toLowerCase().includes(target.entry.toLowerCase())) {
    return { ok: false, message: `bootsequence did not stick (firmware reports ${after.value ?? 'nothing'})` };
  }
  return { ok: true, message: `bootsequence=${target.entry}` };
}

// ---------------------------------------------------------------------------
// grub
// ---------------------------------------------------------------------------

function readGrubNextEntry() {
  let result = runCommand('grub-editenv', ['list'], { timeoutMs: 15000 });
  if (!result.ok) result = runCommand('sudo', ['-n', 'grub-editenv', 'list'], { timeoutMs: 15000 });
  if (!result.ok) return { ok: false, value: null, message: describeFailure(result) };
  const match = /^next_entry=(.*)$/m.exec(result.stdout);
  return { ok: true, value: match ? match[1].trim() : null, message: null };
}

function armGrubReboot(target) {
  const set = runCommand('sudo', ['-n', 'grub-reboot', target.entry], { timeoutMs: 20000 });
  if (!set.ok) return { ok: false, message: `grub-reboot failed: ${describeFailure(set)}` };

  const after = readGrubNextEntry();
  if (!after.ok) return { ok: false, message: `grub-reboot ran but grubenv could not be read back: ${after.message}` };
  if (after.value !== target.entry) {
    return { ok: false, message: `next_entry did not stick (wanted ${target.entry}, grubenv has ${after.value ?? 'nothing'})` };
  }
  return { ok: true, message: `grub next_entry=${target.entry}` };
}

// ---------------------------------------------------------------------------
// Arming
// ---------------------------------------------------------------------------

/**
 * A boot target described entirely by commands.
 *
 * Every other method reads the firmware back after arming it. This one can only
 * read back what the config gives it, so a target with no `verify` command
 * CANNOT be armed for a reboot at all — and --force does not change that. The
 * whole reason arming is verified is that rebooting an unarmed machine returns
 * to the system it was already on while the controller waits for one that never
 * appears; a force flag that skipped the check would be a flag whose only effect
 * is to make that outcome more likely.
 *
 * `boot --no-reboot` still arms it, because nothing is lost by trying when
 * nobody is about to close the session on the strength of it.
 */
function armCommand(target, { requireVerification }) {
  if (!target.verify && requireVerification) {
    return {
      ok: false,
      verified: false,
      message:
        `the boot target ${target.id} has no verify command, so there is no way to confirm it was armed. ` +
        'Add "verify" (a command that fails unless the target really is armed), or use --no-reboot and check it yourself. ' +
        '--force overrides the busy gate, not the evidence',
    };
  }

  const armed = runArgv(target.arm, { timeoutMs: 60000 });
  if (!armed.ok) return { ok: false, verified: false, message: `the arm command failed: ${describeFailure(armed)}` };

  if (!target.verify) {
    return { ok: true, verified: false, message: `${target.id} armed (no verify command is configured, so this was not read back)` };
  }
  const verified = runArgv(target.verify, { timeoutMs: 30000 });
  if (!verified.ok) {
    return { ok: false, verified: false, message: `the arm command ran but verification failed: ${describeFailure(verified)}` };
  }
  return { ok: true, verified: true, message: `${target.id} armed and verified` };
}

/** The boot targets this system can offer, in config order. */
export function bootTargets(config) {
  return Object.values(config.boot?.targets ?? {}).map((target) => ({ id: target.id, name: target.name ?? null }));
}

export function findBootTarget(config, id) {
  return config.boot?.targets?.[id] ?? null;
}

/**
 * Arm the next boot. The reboot itself is the caller's decision.
 *
 * `requireVerification` is true whenever a reboot is going to follow, which is
 * every path except `--no-reboot`. Every firmware method reads itself back
 * regardless; it only changes what a command-described target is allowed to do.
 */
export function armNextBoot(config, id, { requireVerification = true } = {}) {
  const target = findBootTarget(config, id);
  if (!target) return { ok: false, verified: false, message: `unknown boot target: ${id}` };

  switch (target.method) {
    case 'efi-bootnext':
      return { verified: true, ...armEfiBootNext(target) };
    case 'clear-bootsequence':
      return { verified: true, ...armClearBootSequence() };
    case 'bootsequence':
      return { verified: true, ...armBootSequence(target) };
    case 'grub-reboot':
      return { verified: true, ...armGrubReboot(target) };
    case 'command':
      return armCommand(target, { requireVerification });
    default:
      return { ok: false, verified: false, message: `unknown boot method: ${target.method}` };
  }
}

/**
 * Whether non-interactive sudo will actually allow one exact command.
 *
 * `sudo -l <cmd>` asks the policy without running anything, and with -n it never
 * prompts. This is the preflight the first version was missing: the reboot
 * itself is detached and cannot report back, so without asking first a machine
 * that is never going to go down gets reported as rebooting.
 */
function sudoAllows(args) {
  const listed = runCommand('sudo', ['-n', '-l', ...args], { timeoutMs: 10000 });
  if (listed.ok) return { ok: true, message: null };
  // Some sudo builds do not support -l for a specific command by an unprivileged
  // caller. Fall back to proving that non-interactive sudo works at all, and let
  // the real command report its own failure if the rule is narrower than that.
  const any = runCommand('sudo', ['-n', '-v'], { timeoutMs: 10000 });
  if (any.ok) return { ok: true, message: null };
  return { ok: false, message: describeFailure(listed) };
}

/**
 * Reboot a few seconds from now, so this process can still print its JSON object
 * and let an ssh session close cleanly before the machine goes down.
 *
 * Only ever called after arming has been verified.
 *
 * The Linux path is deliberately arranged so that NOTHING PRIVILEGED IS A SHELL.
 * The first version ran `sudo -n sh -c 'sleep 3; systemctl reboot'`, which asks
 * for a root shell — far more authority than "reboot this machine" needs, and
 * more than the documented sudoers entry grants. The delay now lives in an
 * ordinary unprivileged shell owned by this user, and the only thing sudo ever
 * sees is `systemctl reboot` with no arguments of its own:
 *
 *   sh -c 'sleep 3; sudo -n systemctl reboot'      <- unprivileged
 *                   ^^^^^^^^^^^^^^^^^^^^^^^^      <- the only privileged part
 *
 * so the sudoers rule can be exactly:
 *
 *   <user> ALL=(root) NOPASSWD: /usr/bin/systemctl reboot
 */
export function scheduleReboot(config) {
  // A helper the machine's owner wrote, given the narrowest possible privilege.
  // Tried before anything else, because a machine that has one has said how it
  // wants this done.
  const helper = config.boot?.rebootHelper;
  if (helper) {
    const result = runArgv(helper, { timeoutMs: 20000 });
    return {
      ok: result.ok,
      via: 'rebootHelper',
      message: result.ok ? 'the configured reboot helper accepted the request' : describeFailure(result),
    };
  }

  const override = config.boot?.reboot;
  if (override) {
    const result = runArgv(override, { timeoutMs: 20000 });
    return {
      ok: result.ok,
      via: 'boot.reboot',
      message: result.ok ? 'the configured reboot command was accepted' : describeFailure(result),
    };
  }

  const current = detectPlatform();
  if (current === 'windows') {
    const result = runCommand('shutdown', ['/r', '/t', '3'], { timeoutMs: 20000 });
    return { ok: result.ok, via: 'shutdown', message: result.ok ? 'reboot scheduled in 3 s' : describeFailure(result) };
  }
  if (current === 'mac') {
    const allowed = sudoAllows(['/sbin/shutdown', '-r', '+1']);
    if (!allowed.ok) {
      return { ok: false, via: null, message: `passwordless sudo will not run shutdown: ${allowed.message}` };
    }
    const result = runCommand('sudo', ['-n', 'shutdown', '-r', '+1'], { timeoutMs: 20000 });
    return { ok: result.ok, via: 'shutdown', message: result.ok ? 'reboot scheduled in 1 minute' : describeFailure(result) };
  }
  if (current === 'linux') {
    const allowed = sudoAllows(['/usr/bin/systemctl', 'reboot']);
    if (!allowed.ok) {
      return {
        ok: false,
        via: null,
        message:
          `passwordless sudo will not run "systemctl reboot": ${allowed.message}. ` +
          'Add exactly that one command to sudoers, or set boot.rebootHelper to a program of your own',
      };
    }
    try {
      // Unprivileged: this shell belongs to the invoking user and only holds the
      // delay. It is detached so the ssh session can close before the machine
      // goes down, which is the whole reason for the delay.
      const child = spawn('sh', ['-c', 'sleep 3; sudo -n systemctl reboot'], { detached: true, stdio: 'ignore' });
      child.unref();
      return { ok: true, via: 'systemctl', message: 'reboot scheduled in 3 s' };
    } catch (err) {
      return { ok: false, via: null, message: `could not schedule the reboot: ${err.message}` };
    }
  }
  return { ok: false, via: null, message: `no reboot command is configured for ${process.platform}` };
}

// ---------------------------------------------------------------------------
// Sleep
// ---------------------------------------------------------------------------

/** Shared wording for "the suspend command came back": accepted is not slept. */
export function suspendAccepted(what) {
  return `${what} was accepted; the machine should drop off in a moment. Nothing here waits to watch it happen, so this is accepted, not confirmed`;
}

function toolExists(candidate) {
  try {
    return fs.existsSync(candidate);
  } catch {
    // An unreadable path is simply not a candidate.
    return false;
  }
}

function toolOnPath(name) {
  const result =
    process.platform === 'win32'
      ? runCommand('where.exe', [name], { timeoutMs: 10000 })
      : runCommand('which', [name], { timeoutMs: 10000 });
  if (!result.ok) return null;
  const first = result.stdout.split('\n').map((line) => line.trim()).find((line) => line.length > 0);
  return first || null;
}

export const defaultToolProbe = { exists: toolExists, onPath: toolOnPath };

/**
 * Which command actually suspends this machine.
 *
 * The Windows branch is a list rather than one path on purpose: the built in
 * rundll32 and .NET suspend calls are silently vetoed on some machines. They
 * exit 0, report success, and the machine keeps running, so the tool has to be
 * resolved at run time and never assumed.
 *
 * `-t 3`, not `-t 0`. With a zero delay psshutdown never returns in a non
 * interactive session and the machine stays awake; with a short delay it
 * schedules the suspend, prints that it has, and exits straight away. The three
 * seconds also let this process print its JSON and an ssh session close before
 * the machine goes down.
 */
export function selectSleepCommand(platform, sleepConfig = {}, probe = defaultToolProbe) {
  if (sleepConfig.command) return { ok: true, command: sleepConfig.command, message: null };
  if (platform === 'linux') {
    // Non-interactive sudo on purpose: an ssh session has nowhere to type a
    // password, and a prompt would hang here until the timeout.
    return { ok: true, command: ['sudo', '-n', 'systemctl', 'suspend'], message: null };
  }
  if (platform === 'mac') {
    // pmset needs no elevation and is the same thing the Apple menu does.
    return { ok: true, command: ['pmset', 'sleepnow'], message: null };
  }
  if (platform === 'windows') {
    const candidates = sleepConfig.tools ?? [];
    for (const candidate of candidates) {
      const resolved = /[\\/]/.test(candidate) ? (probe.exists(candidate) ? candidate : null) : probe.onPath(candidate);
      if (resolved) return { ok: true, command: [resolved, '-d', '-t', '3', '-accepteula'], message: null };
    }
    return {
      ok: false,
      command: null,
      message: `no suspend tool found; looked at ${candidates.join(', ') || 'nothing'}`,
    };
  }
  return { ok: false, command: null, message: `no suspend command is configured for ${platform}` };
}

/**
 * Put the machine to sleep, right now.
 *
 * The `before` commands exist for anything on the machine that holds sleep off
 * while a remote session is open. Such a hold is usually per process and can
 * only be dropped by the process that took it, which is why this is a list of
 * commands to run rather than a flag to unset, and why the settle wait matters:
 * the hold is released as that process exits, which is not instant.
 *
 * The suspend itself runs synchronously and reports what the command said,
 * because that is the only honest signal available: the machine goes down
 * underneath this process and nobody is left running to observe it. A zero exit
 * means "the request was accepted", and the caller must never upgrade that to
 * "the machine slept".
 *
 * A timeout is its own answer and is deliberately not folded into the generic
 * failure text. The command not returning usually means the machine went down
 * while we were waiting, so calling it a plain failure would be as wrong as
 * calling it a success. Say the result is unknown and let the caller look.
 */
export async function suspendMachine(config) {
  const sleepConfig = config.sleep ?? {};
  const chosen = selectSleepCommand(detectPlatform(), sleepConfig);
  if (!chosen.ok) {
    return { ok: false, message: `${chosen.message}; the machine was left running` };
  }

  const warnings = [];
  for (const command of sleepConfig.before ?? []) {
    const result = runArgv(command, { timeoutMs: 20000 });
    if (!result.ok) warnings.push(describeFailure(result));
  }
  if ((sleepConfig.before ?? []).length > 0 && sleepConfig.settle > 0) {
    await sleep(sleepConfig.settle * 1000);
  }

  const result = runArgv(chosen.command, { timeoutMs: 30000 });
  const label = path.basename(chosen.command[0]);
  if (result.ok) {
    const accepted = suspendAccepted(label);
    if (warnings.length > 0) {
      return {
        ok: true,
        message: `${accepted}. Note: ${warnings.join('; ')}, so the machine may stay awake`,
      };
    }
    return { ok: true, message: accepted };
  }
  if (result.timedOut) {
    return { ok: false, message: `${result.command} never returned, so whether the machine is suspending is unknown` };
  }
  return { ok: false, message: describeFailure(result) };
}
