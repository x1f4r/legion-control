import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import test from 'node:test';
import { checkPrivateData } from '../scripts/check-private-data.mjs';

const repository = path.resolve(import.meta.dirname, '..');
const script = path.join(repository, 'scripts', 'check-private-data.mjs');

// The values the check must refuse are put together here at run time. Written out, they would be
// in this file, and the check, which reads every file of the repository, would refuse it.
const user = ['zel', 'da'].join('');
const posixHome = (root) => ['', root, user].join('/');
const address = (...parts) => parts.join('.');

// A git checkout with nothing committed, read as one would be just before a commit, and a home
// directory of its own so the maintainer's real terms file is never what a test reads.
function checkout(t, files) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'legion-private-'));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const tree = path.join(root, 'checkout');
  const home = path.join(root, 'home');
  fs.mkdirSync(home);
  assert.equal(spawnSync('git', ['init', '-q', tree]).status, 0);
  for (const [file, text] of Object.entries(files)) {
    fs.mkdirSync(path.dirname(path.join(tree, file)), { recursive: true });
    fs.writeFileSync(path.join(tree, file), text);
  }
  const terms = (text) => {
    fs.mkdirSync(path.join(home, '.legion-control', 'private'), { recursive: true });
    fs.writeFileSync(path.join(home, '.legion-control', 'private', 'terms.txt'), text);
  };
  const check = (env = {}) => checkPrivateData(tree, { env, home });
  const run = (env = {}) => spawnSync(process.execPath, [script, tree], { encoding: 'utf8', env: { ...process.env, LEGION_PRIVATE_TERMS: '', ...env, HOME: home, USERPROFILE: home } });
  return { tree, home, terms, check, run };
}
const where = (result) => result.findings.map(({ file, line }) => `${file}:${line}`);

test('a home directory is refused unless its user is a placeholder, in every way a path is written', (t) => {
  const { check } = checkout(t, {
    'refused.txt': [
      `cd ${posixHome('home')}/legion-control`,
      `"identityFile": "${posixHome('Users')}/.ssh/id_ed25519"`,
      `$Base = 'D:\\Users\\${user}\\.legion-control'`,
      `{ "path": "C:\\\\Users\\\\${user}\\\\.legion-control" }`,
      `c:/users/${user}/AppData`,
      // The same homes through a mount: WSL's view of a Windows drive, and an image-based Fedora.
      `cp ${posixHome('mnt/c/Users')}/Downloads/x .`,
      `IdentityFile ${posixHome('var/home')}/.ssh/id_ed25519`,
    ].join('\n'),
    'placeholders.txt': [
      '/home/me/legion-control and /Users/me/.ssh/id and C:\\Users\\me\\x and C:\\\\Users\\\\me and C:/Users/me/x.',
      '/mnt/c/Users/me/Downloads, /mnt/d/users/Public/x and /var/home/me/.ssh.',
      'everything lives under /home/me.',
      '/home/runner/work, /Users/runner/work, C:\\Users\\runneradmin\\work, /Users/Shared/x, C:\\Users\\Public\\x',
      // Spelled so that they name nobody: a placeholder, a variable, an interpolation.
      'ssh <machine> bash ~/x; /home/<you>/x; /home/$USER/x; /Users/${name}/x; "/Users/\\(user)/x"; C:\\Users\\<you>\\x',
      // A path in a web address is not a home directory.
      `https://example.com/home/${user} and https://api.github.com/users/${user} and https://example.com/var/home/${user}`,
    ].join('\n'),
  });
  const result = check();
  assert.deepEqual(where(result), ['refused.txt:1', 'refused.txt:2', 'refused.txt:3', 'refused.txt:4', 'refused.txt:5', 'refused.txt:6', 'refused.txt:7']);
  assert.match(result.findings[0].message, new RegExp(`^/home/${user} is somebody's home directory; write it for the placeholder user me\\.$`));
  assert.match(result.findings[3].message, new RegExp(`^C:\\\\\\\\Users\\\\\\\\${user} is`));
  assert.match(result.findings[5].message, new RegExp(`^/mnt/c/Users/${user} is`));
  assert.match(result.findings[6].message, new RegExp(`^/var/home/${user} is`));
});

test('a private address is refused outside the placeholder ranges, a site prefix too', (t) => {
  const { check } = checkout(t, {
    'refused.json': [
      `"host": "${address(192, 168, 77, 20)}"`,
      `"lanPrefixes": ["${address(192, 168, 77)}."]`,
      `"broadcast": ["${address(10, 1, 255, 255)}"]`,
      `docker gives ${address(172, 20, 0, 1)}`,
      `"::ffff:${address(192, 168, 77, 1)}"`,
      `ping ${address(10, 0, 1, 7)}/24`,
    ].join('\n'),
    'allowed.json': [
      // The placeholder ranges, and addresses that are nobody's network.
      '10.0.0.5 10.0.0.255 192.168.1.7 192.168.10.0/24 192.168.50.255 "192.168.1." "192.168.50" 192.168.01.',
      '192.0.2.10 198.51.100.7 203.0.113.9 100.100.100.100 172.32.0.1 172.15.0.1 8.8.8.8 127.0.0.1',
      '10.0.2.2 172.16.0.0/12 192.168.0.0/16 192.168.16.0/20 192.168.100.7 192.168.11.1',
      // Versions and build numbers, and an address that cannot be one.
      'build 10.1.2.3.4, 172.20.1.15.2, Windows 10.0.26100.1, gradle 9.5.1, 192.168.1.256, v192.168.77.20',
    ].join('\n'),
  });
  const result = check();
  assert.deepEqual(where(result), ['refused.json:1', 'refused.json:2', 'refused.json:3', 'refused.json:4', 'refused.json:5', 'refused.json:6']);
  assert.equal(result.findings[1].message.split(' ')[0], address(192, 168, 77));
  assert.match(result.findings[0].message, /is on a private network; use an address from 192\.0\.2\.0\/24/);
});

test('a file\'s name is read as a line is, so a fixture named for an address gives it away too', (t) => {
  const named = `contract/fixtures/wake-${address(192, 168, 77, 20)}.json`;
  const { check } = checkout(t, {
    [named]: '{}\n',
    'contract/fixtures/wake-192.168.50.40.json': '{}\n',
  });
  const result = check();
  assert.deepEqual(where(result), [`${named}:0`]);
  assert.equal(result.findings[0].message.split(' ')[0], address(192, 168, 77, 20));
});

test('private terms come from the environment and the file outside the repository, and are never printed', (t) => {
  const { tree, terms, check, run } = checkout(t, {
    'docs/a.md': 'first line\nwritten on Zebulon\'s laptop\n',
    'docs/b.md': 'nothing\nnothing\nthe ALBATROSS tower\n',
    'quux-host notes.txt': 'clean\n',
    'docs/c.md': 'all clean\n',
  });
  const env = { LEGION_PRIVATE_TERMS: 'zebulon\n# a comment, not a term\nnothing-here, Quux-Host' };
  assert.deepEqual(where(check(env)), ['docs/a.md:2', 'quux-host notes.txt:0']);
  terms('# kept outside the repository\nAlbatross\n');
  const both = check(env);
  assert.deepEqual(both.sources, ['LEGION_PRIVATE_TERMS', '~/.legion-control/private/terms.txt']);
  assert.deepEqual(where(both).sort(), ['docs/a.md:2', 'docs/b.md:3', 'quux-host notes.txt:0']);
  assert.deepEqual(both.findings.map(finding => finding.message).sort(), [
    'holds entry 1 of LEGION_PRIVATE_TERMS.',
    'holds entry 1 of ~/.legion-control/private/terms.txt.',
    'its name holds entry 3 of LEGION_PRIVATE_TERMS.',
  ]);

  // The command line, as CI runs it: where, and which entry, and never the term itself.
  fs.rmSync(path.join(tree, 'quux-host notes.txt'));
  const printed = run(env);
  assert.equal(printed.status, 1);
  assert.equal(printed.stdout, 'Looked for 4 private terms from LEGION_PRIVATE_TERMS and ~/.legion-control/private/terms.txt.\n');
  assert.equal(printed.stderr, [
    'docs/a.md:2: holds entry 1 of LEGION_PRIVATE_TERMS.',
    'docs/b.md:3: holds entry 1 of ~/.legion-control/private/terms.txt.',
    '2 private values in 2 of 3 files. The repository is public: write placeholders instead.',
    '',
  ].join('\n'));
  assert.doesNotMatch(printed.stdout + printed.stderr, /zebulon|albatross/i);
});

test('a term is let through in the public places its entry names, and refused everywhere else', (t) => {
  // The account that owns the repository, which its web addresses and the Android package have to
  // name, and which must not come back as the user of a fixture.
  const { check, run } = checkout(t, {
    'README.md': [
      'git clone https://github.com/Nimbus/legion-control',
      'ssh nimbus@workstation',
      'see github.com/nimbus/legion-control, which nimbus wrote',
    ].join('\n'),
    'android/app/build.gradle.kts': 'namespace = "com.nimbus.legioncontrol"\n',
    'android/app/src/main/java/com/nimbus/legioncontrol/Main.kt': 'package com.nimbus.legioncontrol\n',
    'contract/tools/make-fixtures.mjs': "const device = { user: 'nimbus' };\n",
    'docs/nimbus-notes.md': 'clean\n',
  });
  const env = { LEGION_PRIVATE_TERMS: 'nimbus ! github.com/nimbus/ ! com.nimbus. ! com/nimbus/ !' };
  const result = check(env);
  assert.deepEqual(where(result).sort(), ['README.md:2', 'README.md:3', 'contract/tools/make-fixtures.mjs:1', 'docs/nimbus-notes.md:0']);
  assert.ok(result.findings.every(finding => /^(its name )?holds entry 1 of LEGION_PRIVATE_TERMS\.$/.test(finding.message)));

  // A place that does not hold its term lets nothing through, and says so without the term.
  const typo = run({ LEGION_PRIVATE_TERMS: 'longer-term\nnimbus ! github.com/nimbos/' });
  assert.equal(typo.status, 2);
  assert.equal(typo.stderr, 'entry 2 of LEGION_PRIVATE_TERMS lets its term through in a place that does not hold the term.\n');
});

test('without terms the check says so and still checks the rest; a term too short to mean anything is refused', (t) => {
  const { terms, run } = checkout(t, { 'a.txt': '/home/me is fine\n', 'b.txt': `${posixHome('home')}\n` });
  const result = run();
  assert.equal(result.status, 1);
  assert.equal(result.stdout, 'Looked for no private terms: LEGION_PRIVATE_TERMS is not set and there is no ~/.legion-control/private/terms.txt. Home directories and addresses were still checked.\n');
  assert.match(result.stderr, /^b\.txt:1: /);

  const short = run({ LEGION_PRIVATE_TERMS: 'longer-term\nab' });
  assert.equal(short.status, 2);
  assert.equal(short.stderr, 'entry 2 of LEGION_PRIVATE_TERMS is shorter than three characters, which would match almost anything.\n');
  terms('x\n');
  assert.equal(run().status, 2);
});

test('what git ignores and what is binary is not read, and a clean checkout passes', (t) => {
  const { tree, run } = checkout(t, {
    '.gitignore': 'build/\n',
    'build/output.txt': `${posixHome('home')}\n`,
    'icon.png': Buffer.concat([Buffer.from([0x89, 0x50, 0x4e, 0x47, 0, 0, 0, 0]), Buffer.from(posixHome('Users'))]),
    'README.md': 'Install into /home/me/legion-control.\n',
  });
  const result = run();
  assert.equal(result.status, 0, result.stderr);
  assert.match(result.stdout, /\nChecked 3 files: no private home directories, addresses or terms\.\n$/);
  assert.equal(fs.existsSync(path.join(tree, 'build', 'output.txt')), true);
});

test('outside a git checkout the check says why in one line, as the syntax check does', (t) => {
  // A source archive has no .git, and git is kept from finding one further up.
  const root = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), 'legion-private-archive-')));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  fs.writeFileSync(path.join(root, 'README.md'), 'clean\n');
  const run = spawnSync(process.execPath, [script, root], { encoding: 'utf8', env: { ...process.env, LEGION_PRIVATE_TERMS: '', HOME: root, USERPROFILE: root, GIT_CEILING_DIRECTORIES: path.dirname(root), LC_ALL: 'C' } });
  assert.equal(run.status, 2);
  assert.equal(run.stdout, '');
  assert.match(run.stderr, /^git could not list the files of .+: fatal: not a git repository/);
  assert.equal(run.stderr.trim().split('\n').length, 1, run.stderr);
});
