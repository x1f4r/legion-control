// The transport family: command lines, the reading of an attempt, the walk
// over routes, the status budget and the dial grammar (v3.md section 15).
//
// Each reference defect below is a mistake a client really shipped or nearly
// did: a bare tilde, carets inside cmd quotes (D10), only the ASCII quote
// doubled for PowerShell (T11, A12), an unexplained exit 255 read as "never
// sent" (M15), remote stderr taken as evidence of where a command stopped.
// Each must be caught by validate.mjs and by the generator. The vector-file
// defects prove that validate.mjs reads a line back and checks the safety
// claims of a classification and a walk with code of its own, so a committed
// vector that is wrong in the same way as its reference is still caught.
// The properties after them are the ones a table of cases cannot state.

import test from 'node:test';
import assert from 'node:assert/strict';
import path from 'node:path';
import { CONTRACT, defect, readJson, referenceDefect, writeJson } from './support.mjs';
import {
  PRE_DISPATCH, ROUTE_LEVEL, classify, hostForm, isPort, isUser, resolveGrammar, serialize, statusBudget,
} from '../tools/transport.mjs';

const vectors = readJson(path.join(CONTRACT, 'vectors', 'transport.json'));
const group = (name) => vectors.groups[name];

// --- the reference -----------------------------------------------------------

referenceDefect('a tilde left bare in a POSIX line', 'tools/transport.mjs',
  'const POSIX_BARE = /^[A-Za-z0-9._\\/:@+,-][A-Za-z0-9._\\/:@+,=-]*$/;',
  'const POSIX_BARE = /^[A-Za-z0-9._\\/:@+,~-][A-Za-z0-9._\\/:@+,=~-]*$/;',
  /posix-tilde-never-bare: tools\/transport\.mjs gives/);

referenceDefect('carets inside cmd quotes', 'tools/transport.mjs',
  'tokens.push(`"${value}${value.match(/\\\\*$/)[0]}"`);',
  'tokens.push(`"${value.replace(/[&|<>()^]/g, \'^$$&\')}${value.match(/\\\\*$/)[0]}"`);',
  /cmd-operators-inside-quotes: tools\/transport\.mjs gives/);

referenceDefect('only the ASCII quote doubled for PowerShell', 'tools/transport.mjs',
  "const PS_QUOTE = /['\\u2018\\u2019\\u201a\\u201b]/g;",
  "const PS_QUOTE = /'/g;",
  /powershell-typographic-quotes: tools\/transport\.mjs gives/);

referenceDefect('an unexplained exit 255 read as never sent', 'tools/transport.mjs',
  "// and the link dropped afterwards (M15).\n    return fail('linkLost', 'unknown');",
  "// and the link dropped afterwards (M15).\n    return fail('linkLost', 'never');",
  /unexplained-255-empty: tools\/transport\.mjs gives/);

referenceDefect('remote stderr read as evidence', 'tools/transport.mjs',
  "const lines = sshLog.split('\\n')",
  "const lines = `${sshLog}\\n${stderr}`.split('\\n')",
  /refusal-in-stderr-only: tools\/transport\.mjs gives/);

referenceDefect('a mutation sent again after a failure that may have run', 'tools/transport.mjs',
  "if (safety === 'mutation' && result.dispatch !== 'never') return { sent, ending: 'stopped' };",
  "if (safety === 'mutation' && result.dispatch === 'acknowledged') return { sent, ending: 'stopped' };",
  /mutation-stops-after-unexplained-255: tools\/transport\.mjs gives/);

referenceDefect('every attempt given the full status budget', 'tools/transport.mjs',
  'const budget = Math.min(20000, Math.floor(remainingMs) - 5000);',
  'const budget = 20000;',
  /third-attempt-after-two-dead-routes: tools\/transport\.mjs gives/);

// --- the committed vectors, against validate.mjs's own reading -----------------

/** Change one case of the committed transport.json in a copy of the contract. */
const editCase = (groupName, name, change) => (dir) => {
  const file = path.join(dir, 'vectors', 'transport.json');
  const data = readJson(file);
  change(data.groups[groupName].find((entry) => entry.name === name));
  writeJson(file, data);
};

defect('the @ dropped from a cmd line',
  /commandLines\/cmd-program-files-x86: the cmd line does not read back: a cmd line starts with @/,
  editCase('commandLines', 'cmd-program-files-x86', (entry) => { entry.expect.line = entry.expect.line.slice(1); }));

defect('a tilde left bare in a committed line',
  /commandLines\/posix-tilde-never-bare: the posix line does not read back: "~" stands outside quotes/,
  editCase('commandLines', 'posix-tilde-never-bare', (entry) => { entry.expect.line = 'node ~/.legion-control/agent/src/index.mjs'; }));

defect('a typographic quote left single in a PowerShell line',
  /commandLines\/powershell-typographic-quotes: the powershell line (?:does not read back|reads back as)/,
  editCase('commandLines', 'powershell-typographic-quotes', (entry) => { entry.expect.line = "& 'node' '\u2018a\u2019\u201ab\u201b'"; }));

defect('a refusal pinned on an element that does not show it',
  /commandLines\/cmd-percent: the refusal percent names element 0/,
  editCase('commandLines', 'cmd-percent', (entry) => { entry.expect.index = 0; }));

defect('never claimed for an exit 255 nothing explains',
  /classifications\/unexplained-255-empty: never is claimed without evidence that the command cannot have run/,
  editCase('classifications', 'unexplained-255-empty', (entry) => { entry.expect.dispatch = 'never'; }));

defect('a walk that resends a mutation after a link was lost',
  /walks\/mutation-stops-after-unexplained-255: attempt 0 may have run and a later one was made anyway/,
  editCase('walks', 'mutation-stops-after-unexplained-255', (entry) => { entry.expect = { sent: [0, 2], ending: 'reply' }; }));

defect('sshHostGuard refusing an IPv4-mapped IPv6 address',
  /hosts\/host-ipv6-mapped-ipv4: a dialable host must also pass sshHostGuard/, (dir) => {
    const file = path.join(dir, 'schemas', 'common.schema.json');
    const data = readJson(file);
    const pattern = data.$defs.sshHostGuard.pattern;
    assert.equal(pattern.split('[0-9A-Fa-f.]*:[0-9A-Fa-f:.]*').length, 2);
    data.$defs.sshHostGuard.pattern = pattern.replace('[0-9A-Fa-f.]*:[0-9A-Fa-f:.]*', '[0-9A-Fa-f]*:[0-9A-Fa-f:]*');
    writeJson(file, data);
  });

// --- coverage: everything the functions can say is pinned by some case --------

test('every refusal a grammar can give is pinned by a command line', () => {
  const pinned = new Set(group('commandLines').map((entry) => entry.expect.refusal).filter(Boolean));
  assert.deepEqual([...pinned].sort(), [
    'argument-too-long', 'control-character', 'double-quote', 'double-space', 'empty-argument', 'exclamation',
    'line-too-long', 'no-command', 'percent', 'posix-backslash', 'stop-parsing', 'too-many-arguments', 'trailing-backslash',
  ]);
  for (const grammarName of ['posix', 'dispatch', 'cmd', 'powershell']) {
    assert.ok(group('commandLines').some((entry) => entry.input.grammar === grammarName && entry.expect.line !== null), grammarName);
  }
});

test('every row of the pre-dispatch table is matched by some logged line', () => {
  const lines = group('classifications').flatMap((entry) => entry.input.sshLog.split('\n').map((line) => line.replace(/\r$/, '')));
  for (const [kind, patterns] of PRE_DISPATCH) {
    for (const pattern of patterns) assert.ok(lines.some((line) => pattern.test(line)), `${kind}: ${pattern}`);
  }
});

test('every kind an attempt can end in is pinned, with both dispatches where both exist', () => {
  const seen = new Set(group('classifications').map((entry) => `${entry.expect.kind}/${entry.expect.dispatch}`));
  for (const pair of [
    'null/acknowledged', 'launchFailed/never', 'timedOut/unknown', 'linkLost/unknown', 'hostKeyUnknown/never',
    'hostKeyChanged/never', 'authenticationFailed/never', 'connectionRefused/never', 'hostUnreachable/never',
    'interpreterMissing/never', 'interpreterMissing/unknown', 'agentMissing/unknown', 'unreadableOutput/unknown',
  ]) assert.ok(seen.has(pair), pair);
});

test('Android runs exactly the classifications without an ssh log or a launch step', () => {
  for (const entry of group('classifications')) {
    assert.equal(entry.consumers.includes('android'), entry.input.sshLog === '' && !entry.input.launchFailed, entry.name);
  }
});

test('the agent runs exactly the dispatch lines it will be sent', () => {
  for (const entry of group('commandLines')) {
    assert.equal(entry.consumers.includes('agent'), entry.input.grammar === 'dispatch' && entry.expect.line !== null, entry.name);
  }
});

// --- properties ---------------------------------------------------------------

/**
 * sh's reading of a line made of words, '…' runs and \' (enough for every line
 * serialize writes), and every character it read outside quotes.
 */
function shellWords(line) {
  const words = [];
  let word = null;
  let bare = '';
  for (let index = 0; index < line.length;) {
    const character = line[index];
    if (character === ' ') {
      if (word !== null) words.push(word);
      word = null;
      index += 1;
      continue;
    }
    word ??= '';
    if (character === "'") {
      const close = line.indexOf("'", index + 1);
      word += line.slice(index + 1, close);
      index = close + 1;
    } else if (line.startsWith("\\'", index)) {
      word += "'";
      index += 2;
    } else {
      word += character;
      bare += character;
      index += 1;
    }
  }
  if (word !== null) words.push(word);
  return { words, bare };
}

// Characters every shell treats differently, in every pair, so a quoting rule
// that is right for one character and wrong next to another shows up.
const ALPHABET = ['a', ' ', "'", '"', '\\', '$', '`', '~', '=', '-', '*', '!', '#', '%', '^', '&', '|', ';', '(', ')', '{', '}', '<', '>', '?', '@', ',', ':', '/', '.', 'é', '\u2019'];

test('every POSIX and dispatch line reads back as its argv, pair by pair', () => {
  for (const a of ALPHABET) {
    for (const b of ALPHABET) {
      for (const value of [`${a}${b}`, `x${a}${b}x`]) {
        for (const grammarName of ['posix', 'dispatch']) {
          const { line } = serialize(grammarName, ['node', value]);
          if (line === null) {
            assert.equal(grammarName, 'posix', JSON.stringify(value));
            assert.match(value, /\\(?:\\|'|$)/, 'only the fish backslash sequences are refused');
            continue;
          }
          const { words, bare } = shellWords(line);
          assert.deepEqual(words, ['node', value], `${grammarName} ${JSON.stringify(value)} -> ${line}`);
          assert.match(bare, /^[A-Za-z0-9._/:@+,= -]*$/, `only plain characters stand outside quotes: ${line}`);
        }
      }
    }
  }
});

test('a cmd or PowerShell line either refuses an element or quotes every element', () => {
  for (const a of ALPHABET) {
    for (const b of ALPHABET) {
      const cmd = serialize('cmd', ['node', `${a}${b}`]);
      if (cmd.line !== null) assert.match(cmd.line, /^@"node" "[^"]+"$/, JSON.stringify(`${a}${b}`));
      else assert.ok(['double-quote', 'percent', 'exclamation'].includes(cmd.refusal), cmd.refusal);
      const powershell = serialize('powershell', ['node', `${a}${b}`]);
      if (powershell.line !== null) assert.ok(powershell.line.startsWith("& 'node' "), powershell.line);
      else assert.ok(['double-quote', 'trailing-backslash', 'double-space', 'stop-parsing'].includes(powershell.refusal), powershell.refusal);
    }
  }
});

test('a restricted system is dispatch on every platform and shell, or refused for its shape', () => {
  for (const platform of ['linux', 'windows', 'mac', null]) {
    for (const shell of ['posix', 'cmd', 'powershell', null, 'pwsh']) {
      for (const agentLength of [1, 2, 3]) {
        const viaDocument = resolveGrammar({ platform, shell, restricted: true, agentLength });
        const viaSession = resolveGrammar({ platform, shell, restricted: false, restrictedSession: true, agentLength });
        const expected = agentLength === 2 ? { grammar: 'dispatch', refusal: null } : { grammar: null, refusal: 'restricted-agent-shape' };
        assert.deepEqual(viaDocument, expected);
        assert.deepEqual(viaSession, expected);
      }
    }
  }
});

test('never is only ever claimed on evidence the command cannot have run', () => {
  const stdouts = ['', ' \r\n', 'x', '{"ok":true}', '{oops}'];
  const logs = ['', 'ssh: connect to host h port 22: Connection refused\n', 'Connection to h closed by remote host.\n'];
  for (const exitCode of [null, 0, 1, 3, 126, 127, 255, 9009]) {
    for (const stdout of stdouts) {
      for (const sshLog of logs) {
        for (const timedOut of [false, true]) {
          const result = classify({ launchFailed: false, timedOut, exitCode, stdout, stderr: 'Cannot find module x', sshLog });
          if (result.dispatch === 'never') {
            assert.ok(/^[\t\n\r ]*$/.test(stdout) && !timedOut, JSON.stringify({ exitCode, stdout, sshLog, timedOut }));
            assert.ok(exitCode === 255 ? sshLog !== '' : [126, 127, 9009].includes(exitCode));
          }
          assert.equal(result.routeLevel, result.outcome === 'failure' && ROUTE_LEVEL.has(result.kind));
        }
      }
    }
  }
});

test('a status budget is between 1 and 20 seconds, leaves 5 seconds in hand, and never stops a first attempt', () => {
  for (let remainingMs = -1000; remainingMs <= 40000; remainingMs += 250) {
    for (const attemptsMade of [0, 1, 2]) {
      const { attempt, budgetMs } = statusBudget(remainingMs, attemptsMade);
      if (attemptsMade === 0) assert.equal(attempt, true);
      if (!attempt) assert.equal(budgetMs, null);
      else {
        assert.ok(budgetMs >= 1000 && budgetMs <= 20000);
        if (budgetMs > 1000) assert.ok(budgetMs <= remainingMs - 5000);
      }
    }
  }
});

test('the dial grammar is stricter than the guard, never looser', () => {
  const guard = (pattern) => new RegExp(pattern, 'u');
  const common = readJson(path.join(CONTRACT, 'schemas', 'common.schema.json'));
  const hostGuard = guard(common.$defs.sshHostGuard.pattern);
  const userGuard = guard(common.$defs.sshUser.pattern);
  for (const entry of group('hosts')) {
    if (hostForm(entry.input.value) !== null) assert.ok(hostGuard.test(entry.input.value), entry.input.value);
  }
  for (const entry of group('users')) assert.equal(isUser(entry.input.value), userGuard.test(entry.input.value), entry.input.value);
  for (const value of [1, 22, 65535, 0, 65536, -1, 1.5, '22', null, NaN]) assert.equal(isPort(value), Number.isInteger(value) && value >= 1 && value <= 65535);
});
