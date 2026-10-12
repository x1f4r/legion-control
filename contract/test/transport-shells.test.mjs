// Command lines through real shells (v3.md section 15.2).
//
// The vectors pin the bytes of every line, and validate.mjs reads them back
// with decoders of its own. Neither is a shell. This file hands lines written
// by tools/transport.mjs to the programs a client's ssh session really ends
// in, runs node through them, and asks node which arguments arrived. A line
// that every decoder accepts and one real shell reads differently is exactly
// the bug the clients shipped: carets that cmd kept inside quotes (D10), a
// typographic quote that ended a PowerShell string (T11, A12), a backslash
// fish read as an escape.
//
// The interpreter itself lives under a directory called "a b (x86)", so its
// own path needs quoting, as "C:\Program Files (x86)\nodejs\node.exe" does.
// Each shell that is not installed is skipped with the reason, never failed:
// a developer Mac has sh, bash, zsh and ksh, and CI installs the rest (dash,
// fish on Linux; cmd.exe and PowerShell on Windows).

import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { CONTRACT, readJson } from './support.mjs';
import { serialize } from '../tools/transport.mjs';

const WINDOWS = process.platform === 'win32';

// What the remote side has to hand to node unchanged, per grammar: every
// character a shell treats as syntax, spaces in every position, quotes of every
// kind, a backslash where each grammar allows one, and text that is not ASCII.
const SHARED = [
  'plain', '-c', '--flag=value', 'a=b', 'two words', ' leading space', 'trailing space ', "it's", '$HOME', '${HOME}',
  '$(id)', ';', '|', '<in', '>out', '*', '?', '[a]', '{a,b}', '~', '~/x', '#comment', '^', '(x)', 'naïve', 'emoji 🙂',
  '\u2018quoted\u2019', 'C:\\Program Files\\nodejs\\node.exe',
];
const PAYLOAD = {
  posix: [...SHARED, '=x', 'double  space', "''", '"double"', '`id`', '&', '!', '%PATH%', 'a\\b', '@scope/pkg', 'x,y:z+w'],
  cmd: [...SHARED, 'double  space', 'a&b|c<d>e^f', 'C:\\stage dir\\', 'trail\\\\', '@(1)'],
  powershell: [...SHARED, '$env:USERPROFILE', '@(1)', '$(1+1)', '`n', '&', 'a\\b', '\u201aand\u201b', "''"],
};

// Every argv the vectors pin for a grammar is sent through it as well.
const vectors = readJson(path.join(CONTRACT, 'vectors', 'transport.json'));
const pinned = (grammar) => vectors.groups.commandLines
  .filter((entry) => entry.input.grammar === grammar && entry.expect.line !== null && Buffer.byteLength(entry.expect.line) < 1024)
  .flatMap((entry) => entry.input.argv);

let workspace;
let interpreter;
let printer;
let home;

test.before(() => {
  workspace = fs.mkdtempSync(path.join(os.tmpdir(), 'legion-shells-'));
  const spaced = path.join(workspace, 'a b (x86)');
  fs.mkdirSync(spaced);
  interpreter = path.join(spaced, WINDOWS ? 'node.exe' : 'node');
  if (WINDOWS) {
    // A hard link, or a copy on another volume: a symbolic link needs a
    // privilege there that CI does not have.
    try {
      fs.linkSync(process.execPath, interpreter);
    } catch {
      fs.copyFileSync(process.execPath, interpreter);
    }
  } else {
    // A symbolic link, because a node that loads its library relative to its
    // own location (Homebrew's does) only finds it through the real path.
    fs.symlinkSync(process.execPath, interpreter);
  }
  // Non-ASCII is written as \u escapes so a shell that re-encodes what a
  // program prints, as Windows PowerShell does, cannot change the answer.
  printer = path.join(spaced, 'print argv.cjs');
  fs.writeFileSync(printer, [
    "const text = JSON.stringify(process.argv.slice(2)).replace(/[\\u007f-\\uffff]/g,",
    "  (c) => '\\\\u' + c.charCodeAt(0).toString(16).padStart(4, '0'));",
    'process.stdout.write(text);',
    '',
  ].join('\n'));
  home = path.join(workspace, 'home');
  fs.mkdirSync(path.join(home, '.config'), { recursive: true });
});

test.after(() => {
  fs.rmSync(workspace, { recursive: true, force: true });
});

/** The executable for `name` on PATH, or null. */
function onPath(name) {
  for (const directory of (process.env.PATH ?? '').split(path.delimiter)) {
    if (directory === '') continue;
    const candidate = path.join(directory, name);
    try {
      if (fs.statSync(candidate).isFile()) {
        if (!WINDOWS) fs.accessSync(candidate, fs.constants.X_OK);
        return candidate;
      }
    } catch {
      // not here
    }
  }
  return null;
}

/**
 * The environment a shell runs in: this PATH, a home with no startup files
 * (zsh and fish read some even for -c), and a UTF-8 locale.
 */
function environment() {
  const env = { PATH: process.env.PATH ?? '', HOME: home, XDG_CONFIG_HOME: path.join(home, '.config'), LC_ALL: process.platform === 'darwin' ? 'en_US.UTF-8' : 'C.UTF-8' };
  if (WINDOWS) for (const key of ['SystemRoot', 'SYSTEMROOT', 'ComSpec', 'PATHEXT', 'TEMP', 'TMP', 'USERPROFILE', 'windir']) if (process.env[key]) env[key] = process.env[key];
  return env;
}

/** Run `line` through a shell and return the arguments node received. */
function argumentsThrough(executable, args, options = {}) {
  const result = spawnSync(executable, args, { encoding: 'utf8', env: environment(), timeout: 60000, ...options });
  assert.equal(result.error, undefined, String(result.error));
  assert.equal(result.status, 0, `exit ${result.status}\nstdout: ${result.stdout}\nstderr: ${result.stderr}`);
  return JSON.parse(result.stdout.trim());
}

function lineFor(grammar, payload) {
  const { line, refusal, index } = serialize(grammar, [interpreter, printer, ...payload]);
  assert.equal(refusal, null, `${grammar} refused element ${index}: ${JSON.stringify([interpreter, printer, ...payload][index])}`);
  return line;
}

// --- POSIX shells -------------------------------------------------------------

for (const shell of ['sh', 'dash', 'bash', 'zsh', 'ksh', 'fish']) {
  test(`${shell} -c runs a posix line with exactly its argv`, (t) => {
    // A Windows system is driven through cmd or PowerShell. Git for Windows
    // puts a bash on PATH whose runtime rewrites anything that looks like a
    // POSIX path before node sees it, which says nothing about a login shell.
    const executable = WINDOWS ? null : onPath(shell);
    if (executable === null) {
      t.skip(WINDOWS ? `${shell} not installed as a POSIX login shell on Windows` : `${shell} not installed`);
      return;
    }
    const payload = [...PAYLOAD.posix, ...pinned('posix')];
    assert.deepEqual(argumentsThrough(executable, ['-c', lineFor('posix', payload)]), payload);
  });
}

// --- cmd ------------------------------------------------------------------------

// Win32-OpenSSH hands a cmd login `cmd /c` followed by the command, sometimes
// wrapped in one more pair of quotes; the @ prefix is what makes both work.
for (const [label, wrap] of [['wrapped in quotes', (line) => `"${line}"`], ['as it is', (line) => line]]) {
  test(`cmd.exe /c runs a cmd line ${label} with exactly its argv`, (t) => {
    const executable = WINDOWS ? onPath('cmd.exe') ?? process.env.ComSpec ?? null : null;
    if (executable === null) {
      t.skip('cmd.exe not installed');
      return;
    }
    const payload = [...PAYLOAD.cmd, ...pinned('cmd')];
    const received = argumentsThrough(executable, ['/d', '/c', wrap(lineFor('cmd', payload))], { windowsVerbatimArguments: true });
    assert.deepEqual(received, payload);
  });
}

// --- PowerShell -------------------------------------------------------------------

for (const shell of ['powershell.exe', 'pwsh']) {
  test(`${shell} -c runs a powershell line with exactly its argv`, (t) => {
    const executable = shell === 'pwsh' ? onPath(WINDOWS ? 'pwsh.exe' : 'pwsh') : WINDOWS ? onPath(shell) : null;
    if (executable === null) {
      t.skip(`${shell} not installed`);
      return;
    }
    const payload = [...PAYLOAD.powershell, ...pinned('powershell')];
    const received = argumentsThrough(executable, ['-NoProfile', '-NonInteractive', '-Command', lineFor('powershell', payload)]);
    assert.deepEqual(received, payload);
  });
}

test('every payload is one the grammar accepts, so a skip is never hiding a refusal', () => {
  for (const [grammar, payload] of Object.entries(PAYLOAD)) {
    assert.equal(serialize(grammar, ['node', ...payload, ...pinned(grammar)]).refusal, null, grammar);
  }
});
