// The mutation runner: what a deadline stops, what it leaves alone, and what
// happens when it cannot tell (FINDINGS G2).
//
// spawnSync stopped only the command it started. A timed-out action's child went
// on to write its file after the reply said the action "was stopped", after the
// operation had finished and after the next one had the lock. Every test here
// runs real processes, because the property is about processes: whatever a
// timed-out command started is gone before anything after it begins.

import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { setTimeout as delay } from 'node:timers/promises';
import { producedOutcome } from '../src/contract.mjs';
import { withOperation } from '../src/operate.mjs';
import { beginOperation, bootIdentity, readOperation, updateOperation, wireRecord } from '../src/operations.mjs';
import { describeFailure, ledgerPath, powerShellLiteral, runMutation, unsettledWork, windowsPowerShellModulePath, withOperationContext } from '../src/process-runner.mjs';
import { parseProcessStart } from '../src/state.mjs';
import { deadPid, withHome } from './helpers.mjs';
import { CONTRACT } from './vectors.mjs';

const posix = process.platform !== 'win32';
const node = (source) => [process.execPath, '-e', source];
const run = (argv, options) => runMutation(argv[0], argv.slice(1), options);

function alive(pid) {
  try {
    process.kill(pid, 0);
    return true;
  } catch (error) {
    return error.code === 'EPERM';
  }
}

function ledger() {
  try {
    return JSON.parse(fs.readFileSync(ledgerPath(), 'utf8')).commands;
  } catch {
    return [];
  }
}

/**
 * Run `body` with an agent that may signal nothing in a process group: what a
 * member running as another user, or stuck in the kernel, looks like from
 * here. `release` kills a group the runner left and waits until the ledger
 * has let go of it, and whatever is still held at the end is released too, so
 * nothing this process holds reaches the next test.
 */
async function withoutGroupSignals(body) {
  const original = process.kill;
  process.kill = (pid, signal) => {
    if (pid < 0 && signal !== 0) {
      const error = new Error('operation not permitted');
      error.code = 'EPERM';
      throw error;
    }
    return original.call(process, pid, signal);
  };
  const groups = new Set();
  const release = async (group) => {
    try { original.call(process, -group, 'SIGKILL'); } catch { /* already gone */ }
    groups.delete(group);
    for (let tries = 0; tries < 100 && (await unsettledWork()) !== null; tries += 1) await delay(50);
  };
  try {
    return await body({ held: (group) => groups.add(group), release });
  } finally {
    process.kill = original;
    for (const group of groups) await release(group);
  }
}

/** A parent that starts a child which writes `marker` after `ms`, then never exits itself. */
function parentOfLateWriter(marker, ms) {
  const child = `setTimeout(() => require('node:fs').writeFileSync(${JSON.stringify(marker)}, 'too late'), ${ms})`;
  return node(`require('node:child_process').spawn(process.execPath, ['-e', ${JSON.stringify(child)}], { stdio: 'ignore' }); setInterval(() => {}, 1000)`);
}

test('a timed-out command is a failure, and its delayed child cannot write after the result or overlap what follows', async () => {
  await withHome(async (home) => {
    const late = path.join(home, 'late');
    const after = path.join(home, 'after');
    const result = await run(parentOfLateWriter(late, 2500), { timeoutMs: 400 });
    assert.equal(result.ok, false);
    assert.equal(result.timedOut, true);
    assert.equal(result.terminated, true, result.error);
    assert.match(result.error, /did not finish within 400 ms and was stopped/);

    // What a rollback is to an update: the next step, started on the strength
    // of the answer. It must never share the machine with the late writer.
    const next = await run(node(`require('node:fs').writeFileSync(${JSON.stringify(after)}, 'next')`), { timeoutMs: 10000 });
    assert.equal(next.ok, true, next.error ?? next.stderr);
    await delay(3000);
    assert.equal(fs.existsSync(late), false, 'the timed-out command\'s child was stopped with it');
    assert.equal(fs.readFileSync(after, 'utf8'), 'next');
    assert.deepEqual(ledger(), [], 'a command seen to stop leaves nothing in the ledger');
  });
});

test('a deadline is a failure even for a command that would have exited zero', async () => {
  await withHome(async () => {
    const result = await run(node('setTimeout(() => process.exit(0), 3000)'), { timeoutMs: 200 });
    assert.equal(result.ok, false);
    assert.equal(result.timedOut, true);
    assert.equal(result.code, null);
  });
});

test('a start command is done when it exits, and the daemon it leaves holding stdout is not stopped', async () => {
  await withHome(async () => {
    // The daemon keeps writing to the pipe it inherited. spawnSync waited for
    // that pipe for the whole timeout and then reported a timed-out success.
    // On POSIX it stays in the start command's process group, which a deadline
    // would stop; on Windows a child Node did not detach dies with its parent,
    // so there it is detached the way a Windows launcher detaches a daemon.
    const daemon = "setInterval(() => process.stdout.write('tick\\n'), 50)";
    const start = node(`const child = require('node:child_process').spawn(process.execPath, ['-e', ${JSON.stringify(daemon)}], { stdio: ['ignore', 'inherit', 'inherit'], detached: ${!posix} }); child.unref(); process.stdout.write(child.pid + '\\n')`);
    const started = Date.now();
    const result = await run(start, { timeoutMs: 20000 });
    const pid = Number(result.stdout.split('\n')[0]);
    try {
      assert.equal(result.ok, true, result.error ?? result.stderr);
      assert.equal(result.timedOut, false);
      assert.equal(result.terminated, null);
      assert.ok(Date.now() - started < 5000, 'the daemon\'s open pipe is not what the start waits for');
      assert.ok(Number.isInteger(pid) && pid > 0);
      await delay(800);
      assert.equal(alive(pid), true, 'a daemon started on purpose outlives the command that started it');
      assert.deepEqual(ledger(), []);
      assert.equal(await unsettledWork(), null, 'a daemon is not something later mutations wait for');
    } finally {
      if (pid > 0) {
        try { process.kill(pid, 'SIGKILL'); } catch { /* already gone */ }
      }
    }
  });
});

test('a child that ignores SIGTERM is killed after its grace and does not hang the runner', { skip: !posix && 'POSIX signals' }, async () => {
  await withHome(async () => {
    const started = Date.now();
    const result = await run(node("process.on('SIGTERM', () => {}); setInterval(() => {}, 1000)"), { timeoutMs: 300, graceMs: 500 });
    assert.equal(result.timedOut, true);
    assert.equal(result.terminated, true, result.error);
    assert.ok(Date.now() - started < 3000, `took ${Date.now() - started} ms`);
  });
});

test('a tree that cannot be confirmed stopped holds every later mutation until it is gone', { skip: !posix && 'POSIX process groups' }, async () => {
  await withHome(async (home) => {
    await withoutGroupSignals(async ({ held }) => {
      const started = Date.now();
      const result = await run(node('setInterval(() => {}, 1000)'), { timeoutMs: 200, graceMs: 200, settleMs: 300 });
      assert.ok(Date.now() - started < 3000, 'a group that will not go does not hang the runner');
      assert.equal(result.ok, false);
      assert.equal(result.timedOut, true);
      assert.equal(result.terminated, false);
      assert.match(result.error, /could not be confirmed stopped/);

      const [entry] = ledger();
      assert.equal(entry.state, 'unconfirmed');
      assert.equal(entry.group, entry.pid);
      held(entry.group);

      // Durable, so the next agent finds it too, and in force for every command.
      const blocked = await unsettledWork();
      assert.match(blocked.message, /could not be confirmed stopped/);
      assert.equal(blocked.conflict, null, 'nothing names an operation this command did not belong to');
      const marker = path.join(home, 'next');
      const next = await run(node(`require('node:fs').writeFileSync(${JSON.stringify(marker)}, 'ran')`), { timeoutMs: 5000 });
      assert.equal(next.ok, false);
      assert.equal(next.refused, true);
      assert.match(next.error, /^not started: /);
      assert.equal(fs.existsSync(marker), false, 'a refused command never ran');
    });

    await delay(200);
    assert.equal(await unsettledWork(), null, 'gone is gone, and the entry with it');
    assert.deepEqual(ledger(), []);
    assert.equal((await run(node('process.exit(0)'), { timeoutMs: 5000 })).ok, true);
  });
});

test('a command run after a reboot holds the machine even for an operation queued before it', { skip: !posix && 'POSIX process groups' }, async () => {
  await withHome(async () => {
    // A --when-idle request keeps the boot it was queued in. Its commands run
    // in this one, and an entry stamped with the old boot was dropped the
    // moment it was written, guard and all.
    const id = 'queued-before-reboot';
    assert.equal(beginOperation({ id, kind: 'update', service: 'demo', systemId: 'test' }).ok, true);
    updateOperation(id, { bootId: 'boot-before-the-reboot', phase: 'installing' });
    await withoutGroupSignals(async ({ held, release }) => {
      const result = await withOperationContext(id, () => run(node('setInterval(() => {}, 1000)'), { timeoutMs: 200, graceMs: 200, settleMs: 300 }));
      assert.equal(result.terminated, false);
      const [entry] = ledger();
      held(entry.group);
      assert.equal(entry.bootId, bootIdentity(), 'the boot the command ran in');
      assert.equal(entry.opId, id);

      const blocked = await unsettledWork();
      assert.ok(blocked, 'the command is still held');
      assert.equal(blocked.conflict.opId, id);
      assert.equal(blocked.conflict.phase, 'installing');
      assert.equal((await run(node('process.exit(0)'), { timeoutMs: 5000 })).refused, true);
      await release(entry.group);
    });
  });
});

test('an operation whose own command could not be confirmed stopped is unknown, in its reply and its record alike', { skip: !posix && 'POSIX process groups' }, async () => {
  await withHome(async () => {
    const hang = () => run(node('setInterval(() => {}, 1000)'), { timeoutMs: 200, graceMs: 200, settleMs: 300 });
    await withoutGroupSignals(async ({ held, release }) => {
      // What an update does that recovers its service first: the start runs
      // out of time and will not go, and the update then has nothing to do or
      // finds the machine busy. Neither is an ending it can stand behind.
      const deferredAfter = await withOperation({ kind: 'update', service: 'demo', opId: 'unsettled-deferred-01', systemId: 'test' }, async () => {
        assert.equal((await hang()).terminated, false);
        return { ok: true, action: 'deferred', reasonCode: 'busy', message: 'the machine is busy (a turn is running)' };
      });
      held(ledger()[0].group);
      assert.equal(deferredAfter.action, 'failed');
      assert.equal(deferredAfter.reasonCode, 'timed-out');
      assert.match(deferredAfter.message, /^the machine is busy \(a turn is running\), but node from operation unsettled-deferred-01 .*could not be confirmed stopped/);
      assert.equal(deferredAfter.outcome, 'unknown');
      assert.equal(producedOutcome(deferredAfter), 'unknown', 'what the reply is reduced to');
      assert.equal(deferredAfter.op.outcome, 'unknown');
      const stored = wireRecord(readOperation('unsettled-deferred-01'));
      assert.equal(stored.result.action, 'failed');
      assert.equal(stored.result.outcome, 'unknown');
      assert.equal(stored.outcome, 'unknown');
      await release(ledger()[0].group);

      // A failure is stated as unknown and otherwise left as it was.
      const failedAfter = await withOperation({ kind: 'run', actionId: 'say', opId: 'unsettled-failed-01', systemId: 'test' }, async () => {
        assert.equal((await hang()).terminated, false);
        return { ok: false, action: 'failed', reasonCode: 'internal', message: 'say exited 1' };
      });
      held(ledger()[0].group);
      assert.equal(failedAfter.action, 'failed');
      assert.equal(failedAfter.reasonCode, 'internal');
      assert.equal(failedAfter.message, 'say exited 1');
      assert.equal(failedAfter.outcome, 'unknown');
      assert.equal(wireRecord(readOperation('unsettled-failed-01')).outcome, 'unknown');
      await release(ledger()[0].group);
    });
  });
});

test('an operation whose ledger cannot be read at its end keeps the ending its body came to', async () => {
  await withHome(async () => {
    // Replacing the agent goes ahead whatever the ledger says, because it is
    // the way out for a machine the ledger holds, and a ledger nobody can read
    // holds one. Its ending was rewritten as if a command of its own had run
    // out of time: failed, with an unknown outcome, for an agent that had in
    // fact been replaced.
    fs.mkdirSync(path.dirname(ledgerPath()), { recursive: true });
    fs.writeFileSync(ledgerPath(), '{ not json');
    const installed = await withOperation({ kind: 'self-update', opId: 'unreadable-ledger-01', systemId: 'test' }, async () => (
      { ok: true, action: 'installed', reasonCode: null, message: 'agent 3.1.1 installed' }
    ));
    assert.equal(installed.ok, true);
    assert.equal(installed.action, 'installed');
    assert.equal(installed.message, 'agent 3.1.1 installed');
    assert.equal(producedOutcome(installed), 'succeeded');
    const stored = wireRecord(readOperation('unreadable-ledger-01'));
    assert.equal(stored.result.action, 'installed');
    assert.equal(stored.outcome, 'succeeded');

    // Any other operation is refused by a ledger like that before it starts,
    // so here the ledger breaks under the body, after a command that ended.
    fs.rmSync(ledgerPath());
    const ran = await withOperation({ kind: 'run', actionId: 'say', opId: 'unreadable-ledger-02', systemId: 'test' }, async () => {
      assert.equal((await run(node('process.exit(0)'), { timeoutMs: 5000 })).ok, true);
      fs.writeFileSync(ledgerPath(), '{ not json');
      return { ok: true, action: 'ran', reasonCode: null, message: 'say ran' };
    });
    assert.equal(ran.action, 'ran');
    assert.equal(producedOutcome(ran), 'succeeded');
    assert.equal(wireRecord(readOperation('unreadable-ledger-02')).outcome, 'succeeded');
  });
});

test('an operation whose own command could not be confirmed stopped is unknown even when the ledger cannot be read', { skip: !posix && 'POSIX process groups' }, async () => {
  await withHome(async () => {
    await withoutGroupSignals(async ({ held, release }) => {
      // The agent itself still knows about every command of its own it could
      // not see stop, so an unreadable file does not hide one.
      let group = null;
      const result = await withOperation({ kind: 'run', actionId: 'say', opId: 'unreadable-held-01', systemId: 'test' }, async () => {
        assert.equal((await run(node('setInterval(() => {}, 1000)'), { timeoutMs: 200, graceMs: 200, settleMs: 300 })).terminated, false);
        group = ledger()[0].group;
        held(group);
        fs.writeFileSync(ledgerPath(), '{ not json');
        return { ok: true, action: 'ran', reasonCode: null, message: 'say ran' };
      });
      assert.equal(result.action, 'failed');
      assert.equal(result.reasonCode, 'timed-out');
      assert.match(result.message, /^say ran, but node from operation unreadable-held-01 .*could not be confirmed stopped/);
      assert.equal(result.outcome, 'unknown');
      assert.equal(wireRecord(readOperation('unreadable-held-01')).outcome, 'unknown');
      fs.writeFileSync(ledgerPath(), JSON.stringify({ commands: [] }));
      await release(group);
    });
  });
});

test('a command left behind, running or unconfirmed, holds later mutations only while it runs', async () => {
  const { spawn } = await import('node:child_process');
  for (const state of ['running', 'unconfirmed']) {
    await withHome(async () => {
      // What the ledger keeps when an agent dies mid-command, and when a
      // deadline could not be seen to stop one. The first is judged by the
      // command's own process on POSIX, the second, and on Windows both, by
      // its whole tree.
      const orphan = spawn(process.execPath, ['-e', 'setInterval(() => {}, 1000)'], { detached: true, stdio: 'ignore' });
      const entry = {
        id: `left-${state}`, pid: orphan.pid, group: posix ? orphan.pid : null, platform: process.platform, command: 'node',
        state, startedAt: new Date().toISOString(), endedAt: null, agentPid: deadPid(), bootId: bootIdentity(),
        opId: 'orphaned-update-001', kind: 'update', service: 'demo', phase: 'installing', opStartedAt: new Date().toISOString(),
      };
      fs.mkdirSync(path.dirname(ledgerPath()), { recursive: true });
      fs.writeFileSync(ledgerPath(), JSON.stringify({ commands: [entry] }));
      try {
        const blocked = await unsettledWork();
        assert.ok(blocked, `${state}: a live command holds the machine`);
        assert.match(blocked.message, state === 'running' ? /was still running when the agent running it stopped/ : /could not be confirmed stopped/);
        assert.deepEqual(blocked.conflict, { opId: 'orphaned-update-001', kind: 'update', service: 'demo', phase: 'installing', startedAt: entry.opStartedAt });
        assert.equal((await run(node('process.exit(0)'), { timeoutMs: 5000 })).refused, true);
      } finally {
        orphan.kill('SIGKILL');
      }
      await new Promise((resolve) => (orphan.exitCode !== null || orphan.signalCode !== null ? resolve() : orphan.once('exit', resolve)));
      assert.equal(await unsettledWork(), null, `${state}: an ended command lets go`);
      assert.deepEqual(ledger(), []);
    });
  }
});

test('what a command left when its agent died holds the machine on Windows, where the command died with the agent', async () => {
  // The second agent is one whose process id Windows has since handed to this
  // one. Its entry was taken for this agent's own, judged by the command alone,
  // and let go while what the command left was still running.
  for (const [agent, agentPid] of [['a dead agent', deadPid()], ['a dead agent with this one\'s id', process.pid]]) {
    await withHome(async () => {
      // The command has ended, and a child it started is still running. On
      // POSIX the command had a process group of its own and outlived its
      // agent, so it ended by itself and the child is what it meant to leave,
      // as with a start command's daemon. On Windows it ended because its
      // agent did, and the child is whatever it had not finished.
      const leaver = node("const child = require('node:child_process').spawn(process.execPath, ['-e', 'setInterval(() => {}, 1000)'], { detached: true, stdio: 'ignore' }); child.unref(); process.stdout.write(String(child.pid))");
      const startedAt = new Date().toISOString();
      const command = spawnSync(leaver[0], leaver.slice(1), { encoding: 'utf8' });
      const left = Number(command.stdout);
      assert.ok(Number.isInteger(left) && left > 0, command.stderr);
      const entry = {
        id: 'left-by-a-dead-agent', pid: command.pid, group: posix ? command.pid : null, platform: process.platform, command: 'node',
        state: 'running', startedAt, endedAt: null, agentPid, bootId: bootIdentity(),
        opId: 'killed-update-0002', kind: 'update', service: 'demo', phase: 'installing', opStartedAt: startedAt,
      };
      fs.mkdirSync(path.dirname(ledgerPath()), { recursive: true });
      fs.writeFileSync(ledgerPath(), JSON.stringify({ commands: [entry] }));
      try {
        if (posix) {
          assert.equal(await unsettledWork(), null, `${agent}: a command that ended by itself lets go, whatever it left`);
        } else {
          const blocked = await unsettledWork();
          assert.ok(blocked, `${agent}: what the command left behind holds the machine`);
          assert.equal(blocked.conflict.opId, 'killed-update-0002');
        }
      } finally {
        try { process.kill(left, 'SIGKILL'); } catch { /* already gone */ }
      }
      for (let tries = 0; tries < 100 && alive(left); tries += 1) await delay(50);
      assert.equal(await unsettledWork(), null, `${agent}: and lets go once that is gone too`);
      assert.deepEqual(ledger(), []);
    });
  }
});

test('an entry of this agent\'s own, for a command it saw end, is judged by the command alone', async () => {
  await withHome(async () => {
    // The start command exits and leaves its daemon, and the write that
    // takes the entry out of the ledger fails. The command was seen to end, so
    // the daemon is what it meant to leave, on Windows as anywhere.
    const daemon = 'setInterval(() => {}, 1000)';
    const start = node(`const child = require('node:child_process').spawn(process.execPath, ['-e', ${JSON.stringify(daemon)}], { stdio: 'ignore', detached: ${!posix} }); child.unref(); process.stdout.write(String(child.pid))`);
    const rename = fs.renameSync;
    let writes = 0;
    fs.renameSync = (from, to) => {
      if (to === ledgerPath() && (writes += 1) === 3) throw Object.assign(new Error('no space left on device'), { code: 'ENOSPC' });
      return rename.call(fs, from, to);
    };
    let result;
    try {
      result = await run(start, { timeoutMs: 20000 });
    } finally {
      fs.renameSync = rename;
    }
    const pid = Number(result.stdout);
    try {
      assert.equal(result.ok, true, result.error ?? result.stderr);
      assert.ok(Number.isInteger(pid) && pid > 0);
      const [entry] = ledger();
      assert.equal(entry?.state, 'running', 'the entry the failed write left');
      assert.equal(alive(pid), true);
      assert.equal(await unsettledWork(), null, 'its own command ended, and the daemon does not hold the machine');
      assert.deepEqual(ledger(), []);
    } finally {
      if (pid > 0) {
        try { process.kill(pid, 'SIGKILL'); } catch { /* already gone */ }
      }
    }
  });
});

test('a process id handed to a newer process, or a reboot since, does not hold the machine', async () => {
  const { spawn } = await import('node:child_process');
  await withHome(async () => {
    const hour = 60 * 60 * 1000;
    const written = new Date(Date.now() - hour).toISOString();
    // A process started just now, so long after the entry was written that it
    // cannot be the command the entry names, with a child of its own. On
    // Windows an entry nobody saw end is judged by its whole tree, and the
    // child names the entry's process id as its parent; but it was born after
    // the newer process took that id, so it is that process's. It used to
    // hold the machine for as long as it ran.
    const newer = spawn(process.execPath, ['-e', "const child = require('node:child_process').spawn(process.execPath, ['-e', 'setInterval(() => {}, 1000)'], { stdio: 'ignore' }); process.stdout.write(child.pid + '\\n'); setInterval(() => {}, 1000)"], { detached: true, stdio: ['ignore', 'pipe', 'ignore'] });
    let printed = '';
    newer.stdout.setEncoding('utf8');
    for await (const chunk of newer.stdout) {
      printed += chunk;
      if (printed.includes('\n')) break;
    }
    const grandchild = Number(printed.trim());
    assert.ok(Number.isInteger(grandchild) && grandchild > 0);
    const base = { platform: process.platform, command: 'node', endedAt: null, agentPid: deadPid(), opId: null, kind: null, service: null, phase: null, opStartedAt: null };
    fs.mkdirSync(path.dirname(ledgerPath()), { recursive: true });
    fs.writeFileSync(ledgerPath(), JSON.stringify({
      commands: [
        { ...base, id: 'reused', pid: newer.pid, group: posix ? newer.pid : null, state: 'running', startedAt: written, bootId: bootIdentity() },
        { ...base, id: 'rebooted', pid: newer.pid, group: posix ? newer.pid : null, state: 'unconfirmed', startedAt: new Date().toISOString(), bootId: 'an-earlier-boot' },
      ],
    }));
    // In whatever locale the ssh session handed on: ps prints its start time
    // in that, and a time nobody can read never looks like a newer process.
    const locale = { LANG: process.env.LANG, LC_ALL: process.env.LC_ALL, LC_TIME: process.env.LC_TIME };
    Object.assign(process.env, { LANG: 'de_DE.UTF-8', LC_ALL: 'de_DE.UTF-8', LC_TIME: 'de_DE.UTF-8' });
    try {
      assert.equal(await unsettledWork(), null);
    } finally {
      for (const [name, value] of Object.entries(locale)) {
        if (value === undefined) delete process.env[name];
        else process.env[name] = value;
      }
      newer.kill('SIGKILL');
      try { process.kill(grandchild, 'SIGKILL'); } catch { /* already gone */ }
    }
    assert.deepEqual(ledger(), []);
  });
});

test('a start time is read only as ps prints it in the C locale', () => {
  assert.equal(parseProcessStart('Thu Oct 29 10:15:42 2026\n'), new Date(2026, 9, 29, 10, 15, 42).getTime());
  assert.equal(parseProcessStart('  Thu Oct  1 09:05:02 2026  '), new Date(2026, 9, 1, 9, 5, 2).getTime());
  for (const text of ['Do. 29 Okt. 10:15:42 2026', 'jeu. 29 oct. 2026 10:15:42', 'Thu Okt 29 10:15:42 2026', '', null]) {
    assert.equal(parseProcessStart(text), null, String(text));
  }
});

test('a ledger that cannot be read keeps every mutation from starting', async () => {
  await withHome(async (home) => {
    fs.mkdirSync(path.dirname(ledgerPath()), { recursive: true });
    fs.writeFileSync(ledgerPath(), '{ not json');
    const blocked = await unsettledWork();
    assert.match(blocked.message, /command ledger cannot be read/);
    const marker = path.join(home, 'ran');
    const result = await run(node(`require('node:fs').writeFileSync(${JSON.stringify(marker)}, 'ran')`), { timeoutMs: 5000 });
    assert.equal(result.refused, true);
    assert.equal(fs.existsSync(marker), false);
  });
});

test('a command that cannot start leaves nothing behind it', async () => {
  await withHome(async (home) => {
    const result = await runMutation(path.join(home, 'no-such-program'), [], { timeoutMs: 5000 });
    assert.equal(result.ok, false);
    assert.equal(result.timedOut, false);
    assert.match(result.error, /ENOENT/);
    assert.deepEqual(ledger(), []);
  });
});

test('output is read in full when the command exits, and bounded', async () => {
  await withHome(async () => {
    const result = await run(node("process.stdout.write('abcdef'); process.stderr.write('ghijkl'); process.exitCode = 4"), { timeoutMs: 10000, maxBuffer: 3 });
    assert.equal(result.ok, false);
    assert.equal(result.code, 4);
    assert.equal(result.stdout, 'abc');
    assert.equal(result.stderr, 'ghi');
    assert.equal(result.timedOut, false);
  });
});

test('a command the ledger would not record once it had started is stopped, and was not refused', async () => {
  await withHome(async (home) => {
    // The entry went in, and the write that adds its process id did not. The
    // command ran for that moment, so it is a failure whose effect nobody
    // knows, never the "not started" a refusal tells its caller.
    const marker = path.join(home, 'started');
    const rename = fs.renameSync;
    let writes = 0;
    fs.renameSync = (from, to) => {
      if (to === ledgerPath() && (writes += 1) === 2) throw Object.assign(new Error('no space left on device'), { code: 'ENOSPC' });
      return rename.call(fs, from, to);
    };
    let result;
    try {
      result = await run(node(`require('node:fs').writeFileSync(${JSON.stringify(marker)}, 'ran'); setInterval(() => {}, 1000)`), { timeoutMs: 10000, graceMs: 1000 });
    } finally {
      fs.renameSync = rename;
    }
    assert.equal(result.ok, false);
    assert.equal(result.refused, undefined);
    assert.equal(result.timedOut, false);
    assert.equal(result.terminated, true, result.error);
    assert.match(result.error, /^node(\.exe)? was stopped as it started, because the command ledger would not record it$/);
    assert.equal(describeFailure(result), result.error);
    assert.deepEqual(ledger(), [], 'stopped and seen gone, so nothing is held');
  });
});

// The clients consume these lines, and the agent only the way each element is
// quoted, so every line PowerShell is given there is rebuilt from it here.
test('a PowerShell literal quotes every element as the contract\'s PowerShell command lines do', () => {
  const vectors = JSON.parse(fs.readFileSync(path.join(CONTRACT, 'vectors', 'transport.json'), 'utf8'));
  const lines = vectors.groups.commandLines.filter((entry) => entry.input.grammar === 'powershell' && entry.expect.line !== null);
  assert.ok(lines.some((entry) => /[\u2018-\u201b]/.test(entry.input.argv.join(''))), 'the corpus covers the typographic quotes');
  for (const entry of lines) assert.equal(`& ${entry.input.argv.map(powerShellLiteral).join(' ')}`, entry.expect.line, entry.name);
});

test('Windows PowerShell is never handed the module folders PowerShell 7 puts first', () => {
  const windows = 'C:\\Program Files\\WindowsPowerShell\\Modules;C:\\WINDOWS\\system32\\WindowsPowerShell\\v1.0\\Modules';
  const seven = [
    'C:\\Users\\me\\Documents\\PowerShell\\Modules',
    'C:\\Program Files\\PowerShell\\Modules',
    'c:\\program files\\powershell\\7\\Modules',
    'C:\\Program Files\\PowerShell\\7-preview\\Modules\\',
  ];
  assert.equal(windowsPowerShellModulePath([...seven, windows].join(';')), windows);
  // Anything else stays where it was, a folder of one's own included.
  assert.equal(windowsPowerShellModulePath(`D:\\Tools\\Modules;${windows};`), `D:\\Tools\\Modules;${windows}`);
  assert.equal(windowsPowerShellModulePath(windows), windows);
  assert.equal(windowsPowerShellModulePath(undefined), undefined);
});
