// What a restricted key reads about this machine's configuration (contract
// section 13, finding T7).
//
// Every configured argument after the executable is private: a token on a
// command line, a path that names a person. A restricted key can read status,
// busy, op, history, logs, run and update replies, and every one of them used
// to quote configured commands and their output. These tests configure
// canaries, collect every reply a restricted key can get, and look for them.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { checkReply, schemaForCommand } from '../../contract/validate.mjs';
import { commandLabel, describeFailure, jsonParseProblem, runCommand } from '../src/config.mjs';
import { REDACTED, redactReply, valueRedactor } from '../src/redact.mjs';
import { ENTRY, cli, commandService, fixtureConfig, fixtureData, withHome, writeConfig } from './helpers.mjs';

/** Values that must never reach a restricted key. Two are contract words on purpose. */
const SHORT = 'q7x';
const SECRET = 'hunter2pw';
const CANARIES = [SHORT, SECRET, 'canary-path-0042'];

const quote = (value) => `'${String(value).replaceAll("'", "'\\''")}'`;

/** A configured command that prints its arguments to stdout and stderr, and exits as told. */
function talker(home) {
  const file = path.join(home, 'talk.cjs');
  fs.writeFileSync(file, [
    "const [code, ...rest] = process.argv.slice(2);",
    "process.stdout.write(rest.join(' '));",
    "process.stderr.write(`said ${rest.join(' ')}\\n`);",
    'process.exit(Number(code));',
  ].join('\n'));
  return (code, ...rest) => [process.execPath, file, String(code), ...rest];
}

/** The command a reply answers, as contract/fixtures/index.json names it. */
const commandOf = ([command, sub]) => (command === 'config' && (sub === 'set' || sub === 'meta') ? `config ${sub}` : command);

/** Run one command as a restricted key does, and hold the reply to its schema. */
function restricted(home, argv, { stdin = null } = {}) {
  const run = cli(['dispatch'], {
    home,
    stdin,
    env: { LEGIONCTL_RESTRICTED: '1', SSH_ORIGINAL_COMMAND: [process.execPath, ENTRY, ...argv].map(quote).join(' ') },
  });
  assert.ok(run.payload, `${argv.join(' ')}: one JSON object\n${run.stdout}${run.stderr}`);
  const problems = checkReply(schemaForCommand(commandOf(argv), run.payload), run.payload, { where: `restricted ${argv.join(' ')}` });
  assert.deepEqual(problems, [], `${argv.join(' ')} no longer holds once redacted:\n${JSON.stringify(run.payload, null, 2)}`);
  return run.payload;
}

function assertClean(reply, where) {
  const text = JSON.stringify(reply);
  for (const canary of CANARIES) assert.equal(text.includes(canary), false, `${where} shows ${canary}:\n${JSON.stringify(reply, null, 2)}`);
}

function canaryConfig(home) {
  const dir = fixtureData(home, { latest: '2.0.0' });
  const talk = talker(home);
  // Words a client decodes are configured arguments too: "restart", "busy",
  // "failed" and "finished" are redacted from prose and kept where they are
  // an enum value, or no reply would validate.
  const words = ['restart', 'busy', 'failed', 'finished', 'deferred'];
  writeConfig(home, fixtureConfig(dir, {
    services: [commandService(dir, {
      latestVersion: talk(2, ...words, SHORT, `--password=${SECRET}`, '/srv/canary-path-0042'),
      busy: { type: 'command', command: talk(1, SHORT, `--token=${SECRET}`) },
    })],
    actions: [{ id: 'say', name: 'Say', command: talk(0, ...words, SHORT, `--password=${SECRET}`, '/srv/canary-path-0042') }],
  }));
  return { dir, talk };
}

test('a restricted key reads no configured argument in any reply it can get', async () => {
  await withHome(async (home) => {
    canaryConfig(home);

    // The ordinary session sees everything, and leaves it in the records and the log.
    const ran = cli(['run', 'say'], { home }).payload;
    assert.equal(ran.ok, true, ran.message);
    assert.match(ran.output, new RegExp(`${SHORT} --password=${SECRET}`));
    assert.match(cli(['status'], { home }).stdout, new RegExp(SECRET));

    const replies = {
      run: restricted(home, ['run', 'say']),
      update: restricted(home, ['update', '--service', 'demo', '--force']),
      status: restricted(home, ['status']),
      busy: restricted(home, ['busy']),
      op: restricted(home, ['op', ran.op.id]),
      history: restricted(home, ['history']),
      logs: restricted(home, ['logs']),
      opLogs: restricted(home, ['logs', '--op', ran.op.id]),
      doctor: restricted(home, ['doctor']),
      bundle: restricted(home, ['bundle']),
    };
    for (const [where, reply] of Object.entries(replies)) assertClean(reply, where);

    // What a client decodes survived: the run's own action, its record, the
    // service's reason codes and the record's kind.
    assert.equal(replies.run.action, 'ran');
    assert.equal(replies.run.op.state, 'finished');
    assert.equal(replies.run.output.includes(REDACTED), true);
    assert.equal(replies.history.operations.find((entry) => entry.id === ran.op.id).kind, 'run');
    assert.equal(replies.busy.busy, true);
    assert.equal(replies.busy.evidence, 'command');
    assert.match(replies.busy.reason, new RegExp(REDACTED));
    assert.ok(replies.update.op === null || replies.update.op.kind === 'update');

    // Nothing on disk was rewritten for the restricted reader.
    assert.match(JSON.stringify(cli(['op', ran.op.id], { home }).payload), new RegExp(SECRET));
  });
});

test('a config.json that stops parsing is reported by line and column, and its last good copy still redacts', async () => {
  await withHome(async (home) => {
    canaryConfig(home);
    const ran = cli(['run', 'say'], { home }).payload;
    assert.equal(ran.ok, true);
    const good = fs.readFileSync(path.join(home, 'config.json'), 'utf8');
    assert.ok(fs.existsSync(path.join(home, 'config.last-good.json')), 'a mutation keeps the last good copy');

    // A trailing comma right after a private argument: V8 would quote the text around it.
    const broken = good.replace(`"--password=${SECRET}",`, `"--password=${SECRET}",,`);
    assert.notEqual(broken, good);
    fs.writeFileSync(path.join(home, 'config.json'), broken);
    const line = broken.slice(0, broken.indexOf(',,') + 1).split('\n').length;

    for (const reply of [cli(['status'], { home }).payload, restricted(home, ['status'])]) {
      assert.match(reply.config.problems[0].message, new RegExp(`config\\.json does not parse as JSON: line ${line}, column [0-9]+$`));
      assert.equal(JSON.stringify(reply.config).includes(SECRET), false);
    }
    // The current file does not parse; the old record and the log still say
    // what it held, and only the last good copy knows to redact it.
    for (const argv of [['op', ran.op.id], ['history'], ['logs']]) assertClean(restricted(home, argv), argv.join(' '));
  });
});

test('a command of any shape keeps its arguments from a restricted key', async () => {
  await withHome(async (home) => {
    const dir = fixtureData(home, { latest: '2.0.0' });
    const words = ['canary-word-0001', 'canary-word-0002', 'canary-word-0003', 'canary-word-0004', 'canary-word-0005', 'canary-word-0006'];
    const clean = (reply, where) => {
      const text = JSON.stringify(reply);
      for (const word of [SHORT, ...words]) assert.equal(text.includes(word), false, `${where} shows ${word}:\n${JSON.stringify(reply, null, 2)}`);
    };

    // An array that holds something other than strings runs without it, and
    // its output echoes the rest.
    writeConfig(home, fixtureConfig(dir, { actions: [{ id: 'say', name: 'Say', command: [...talker(home)(0, words[0]), 5] }] }));
    assert.match(cli(['run', 'say'], { home }).payload.output, new RegExp(words[0]));
    clean(restricted(home, ['run', 'say']), 'run');

    // A string command is refused with each of its words quoted, to show how to write it.
    writeConfig(home, fixtureConfig(dir, {
      services: [commandService(dir, { rollback: `tool ${SHORT} --password=${words[1]}` })],
      boot: { targets: {}, reboot: `  tool   ${words[2]}` },
      sleep: { before: [`tool --token=${words[3]}`, ['tool', SHORT]] },
    }));
    const own = JSON.stringify(cli(['status'], { home }).payload.config.problems);
    for (const word of words.slice(1, 4)) assert.equal(own.includes(word), true, `the owner is shown ${word}:\n${own}`);
    assert.match(own, /write \[\\"tool\\", \\"q7x\\"/);
    const replies = {
      status: restricted(home, ['status']),
      doctor: restricted(home, ['doctor']),
      bundle: restricted(home, ['bundle']),
    };
    for (const [where, reply] of Object.entries(replies)) clean(reply, where);
    // The executable is still named, so the owner can find the command.
    assert.match(JSON.stringify(replies.status.config.problems), /write \[\\"tool\\", \\"<redacted>\\"/);

    // Where a block, a list or a command belongs, a value of the wrong type
    // is named and never quoted, to anyone.
    for (const shape of [
      { sleep: `tool --token=${words[4]}` },
      { sleep: { before: `tool --token=${words[4]}`, command: { run: `tool ${words[5]}` } } },
      { services: [commandService(dir, { health: `curl --header=${words[4]}` })] },
      { actions: [`tool ${words[5]}`] },
    ]) {
      writeConfig(home, fixtureConfig(dir, shape));
      const { problems } = cli(['status'], { home }).payload.config;
      assert.match(JSON.stringify(problems), /got (a string|an object)/);
      clean(problems, Object.keys(shape)[0]);
    }
  });
});

test('the credentials of a configured URL are redacted even when it does not parse', async () => {
  await withHome(async (home) => {
    const dir = fixtureData(home, { latest: '2.0.0' });
    const user = 'canary-user-0007';
    const password = 'canarypw-777';
    // A port out of range and a space in the host: both refused by the URL parser.
    for (const url of [`https://${user}:${password}@host.example:99999/`, `https://${user}:${password}@host example/`]) {
      writeConfig(home, fixtureConfig(dir, { services: [commandService(dir, { endpoint: { url } })] }));
      const problems = JSON.stringify(cli(['status'], { home }).payload.config.problems);
      assert.match(problems, /services\[0\]\.endpoint\.url/);
      assert.match(problems, /not a usable URL/);
      assert.equal(problems.includes(password), false, `the problem quotes the URL:\n${problems}`);
      for (const command of ['status', 'doctor', 'bundle']) {
        const text = JSON.stringify(restricted(home, [command]));
        for (const canary of [user, password]) assert.equal(text.includes(canary), false, `${command} shows ${canary}:\n${text}`);
      }
    }
  });

  // However the text quotes it, the redactor knows what a URL that does not parse holds.
  const written = 'ftp://some%20one:pa%40ss-word@host:99999/path@x';
  const redact = valueRedactor({ services: [{ endpoint: { url: written } }] });
  for (const text of [written, 'as typed: some one, pa@ss-word', JSON.stringify({ url: written })]) {
    const redacted = redact(text);
    for (const canary of ['some%20one', 'some one', 'pa%40ss-word', 'pa@ss-word']) assert.equal(redacted.includes(canary), false, `${canary} in ${redacted}`);
  }
  // Only the userinfo goes; the host and path are left for the owner to read.
  assert.equal(redact(written), `ftp://${REDACTED}@host:99999/path@x`);
});

test('a reply that cannot be redacted is never sent as it was', async () => {
  await withHome(async (home) => {
    const depth = 1_000_000;
    const deep = `${'['.repeat(depth)}"${SECRET}"${']'.repeat(depth)}`;
    fs.writeFileSync(path.join(home, 'config.json'), `{"configVersion": 3, "nested": ${deep}}\n`);
    for (const argv of [['status'], ['run', 'say']]) {
      const run = cli(['dispatch'], {
        home,
        env: { LEGIONCTL_RESTRICTED: '1', SSH_ORIGINAL_COMMAND: [process.execPath, ENTRY, ...argv].map(quote).join(' ') },
      });
      assert.equal(run.exitCode, 1);
      assert.equal(run.payload.reasonCode, 'internal', run.stdout.slice(0, 400));
      assert.equal(run.payload.message, 'the reply could not be redacted for a restricted session');
      assert.equal(run.stdout.includes(SECRET), false);
      if (argv[0] === 'run') assert.equal(run.payload.outcome, 'unknown');
      assert.deepEqual(checkReply('error.schema.json', run.payload, { where: argv.join(' ') }), []);
    }
  });
});

test('a short value is redacted only as a whole word, a long one wherever it appears', () => {
  const redact = valueRedactor({ actions: [{ command: ['tool', 'on', '-v', 'secretvalue'] }] });
  assert.equal(redact('turn on the lights, online, -v and --verbose'), `turn ${REDACTED} the lights, online, ${REDACTED} and --verbose`);
  assert.equal(redact('xsecretvaluex'), `x${REDACTED}x`);
  assert.equal(redact('on'), REDACTED);
  // Identifiers and enum words in a reply are never touched, whatever they collide with.
  const reply = redactReply({ kind: 'restart', message: 'restart deferred', state: 'on', op: { phase: 'on', detail: 'on' } }, valueRedactor({ a: ['x', 'restart', 'on'] }));
  assert.deepEqual(reply, { kind: 'restart', message: `${REDACTED} deferred`, state: 'on', op: { phase: 'on', detail: REDACTED } });
});

test('a failed command is named by its executable, never its arguments', () => {
  assert.equal(commandLabel('/usr/local/bin/tool'), 'tool');
  const failed = runCommand(process.execPath, ['-e', 'process.exit(3)', '--', `--password=${SECRET}`]);
  assert.equal(failed.command, path.basename(process.execPath));
  assert.equal(describeFailure(failed), `${path.basename(process.execPath)} failed (exit 3)`);
  assert.equal(describeFailure({ ...failed, timedOut: true }).includes(SECRET), false);
});

test('a JSON parse problem gives a line and a column, and never the text', () => {
  const problem = (text) => {
    try {
      JSON.parse(text);
    } catch (error) {
      return jsonParseProblem(error, text);
    }
    return null;
  };
  assert.equal(problem('{\n  "a": 1,\n  "b": secret\n}'), 'line 3, column 8');
  assert.equal(problem('{\n  "a": [1, 2,]\n}'), 'line 2, column 14');
  assert.equal(problem('{\n  // a comment\n  "a": 1\n}'), 'line 2, column 3');
  assert.equal(problem('{"a": 1}}'), 'line 1, column 9');
  assert.equal(problem('{"a": tr'), 'the JSON ends early');
  assert.equal(jsonParseProblem(new Error('something else'), 'xyz'), 'line 1, column 1');
});
