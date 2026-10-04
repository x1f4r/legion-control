import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import test from 'node:test';
import { createSigningKey, verifyManifest } from '../scripts/release-integrity.mjs';
import { edit, versionTree } from './fixtures/version-tree.mjs';

const repository = path.resolve(import.meta.dirname, '..');
// release.sh needs PlistBuddy, ditto and the BSD sed it edits the Gradle file with: a Mac.
const mac = { skip: process.platform !== 'darwin' };

// gh, as far as release.sh uses it, with its releases and CI runs in a JSON file. It notes every
// change it makes on GitHub in the same log the builds and origin's post-receive hook write to, so
// a test can read the order things happened in.
const GH = `#!/usr/bin/env node
import fs from 'node:fs';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
const file = process.env.LEGION_TEST_GH;
const state = JSON.parse(fs.readFileSync(file, 'utf8'));
const save = () => fs.writeFileSync(file, JSON.stringify(state, null, 2));
const note = (line) => fs.appendFileSync(process.env.LEGION_TEST_EVENTS, line + '\\n');
const fail = (message) => { process.stderr.write(message + '\\n'); process.exit(1); };
const print = (value) => process.stdout.write((typeof value === 'string' ? value : JSON.stringify(value)) + '\\n');
const args = process.argv.slice(2);
const valued = new Set(['--commit', '--limit', '--json', '--jq', '--target', '--title', '--notes-file', '--tag']);
const flags = {};
const words = [];
for (let at = 0; at < args.length; at += 1) {
  if (valued.has(args[at])) flags[args[at]] = args[++at];
  else if (args[at].startsWith('--')) { const [flag, value = 'true'] = args[at].split('='); flags[flag] = value; }
  else words.push(args[at]);
}
const [noun, verb, tag, ...files] = words;
const find = () => state.releases.find(release => release.tagName === tag);
const asset = (file) => ({ name: path.basename(file), size: fs.statSync(file).size });
if (noun === 'auth') process.exit(0);
if (noun === 'run' && verb === 'list') { print(state.runs.filter(run => run.headSha === flags['--commit'])); process.exit(0); }
if (noun === 'release' && verb === 'list') { print(state.releases.map(({ tagName, isDraft, isPrerelease }) => ({ tagName, isDraft, isPrerelease }))); process.exit(0); }
if (noun === 'release' && verb === 'view') {
  const release = find() ?? fail('release not found');
  print(flags['--jq'] === '.url' ? release.url : { assets: release.assets });
  process.exit(0);
}
if (noun === 'release' && verb === 'create') {
  note(\`gh release create \${tag} draft=\${flags['--draft']} target=\${flags['--target']} assets=\${files.length}\`);
  if (state.fail.create === 'before') fail('HTTP 502: the release could not be created');
  if (find()) fail('Release.tag_name already exists');
  state.releases.push({ tagName: tag, isDraft: flags['--draft'] === 'true', isPrerelease: false, title: flags['--title'], notes: fs.readFileSync(flags['--notes-file'], 'utf8'), assets: [], url: \`https://github.com/x1f4r/legion-control/releases/tag/\${tag}\` });
  // A connection that drops half way leaves a draft with some of the assets on it.
  find().assets = (state.fail.create === 'partial' ? files.slice(0, 3) : files).map(asset);
  save();
  if (state.fail.create === 'partial') fail('HTTP 502: an asset upload failed');
  process.exit(0);
}
if (noun === 'release' && verb === 'upload') {
  note(\`gh release upload \${tag} clobber=\${flags['--clobber']} assets=\${files.length}\`);
  const release = find() ?? fail('release not found');
  for (const uploaded of files.map(asset)) release.assets = release.assets.filter(other => other.name !== uploaded.name).concat(uploaded);
  save();
  process.exit(0);
}
if (noun === 'release' && verb === 'edit') {
  note(\`gh release edit \${tag} draft=\${flags['--draft']}\`);
  const release = find() ?? fail('release not found');
  if (state.fail.publish) fail('HTTP 502: the release could not be published');
  if (flags['--verify-tag'] === 'true') {
    const remote = spawnSync('git', ['ls-remote', process.env.LEGION_TEST_ORIGIN, \`refs/tags/\${flags['--tag']}\`], { encoding: 'utf8' });
    if (!remote.stdout.trim()) fail(\`tag \${flags['--tag']} doesn't exist in the repo, aborting due to --verify-tag flag\`);
  }
  release.isDraft = flags['--draft'] === 'true';
  save();
  process.exit(0);
}
fail('the gh stand-in does not know: gh ' + args.join(' '));
`;

// A checkout at 1.3.4 (build 14), released and tagged as v1.3.4 on an origin of its own, with the
// release scripts under test and every build replaced by something that writes what its real
// counterpart would, from the version numbers it finds.
function releaseCheckout(t) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'legion release publish '));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const repo = versionTree(path.join(root, 'repo'));
  const events = path.join(root, 'events');
  const ghState = path.join(root, 'gh.json');
  const origin = path.join(root, 'origin.git');
  // The mode is set apart from the write: the version tree already wrote some of these files.
  const write = (file, text, mode = 0o644) => {
    fs.mkdirSync(path.dirname(file), { recursive: true });
    fs.writeFileSync(file, text);
    fs.chmodSync(file, mode);
  };
  const note = (line) => `printf '%s\\n' "${line}" >> "$LEGION_TEST_EVENTS"\n`;

  for (const file of ['release.sh', 'check-versions.mjs', 'sign-release.mjs', 'release-integrity.mjs', 'release-state.mjs']) {
    fs.copyFileSync(path.join(repository, 'scripts', file), path.join(repo, 'scripts', file));
  }
  write(path.join(repo, 'scripts', 'check.sh'), '#!/bin/sh\n' + note('check.sh $*'), 0o755);
  write(path.join(repo, 'scripts', 'verify-desktop-artifacts.mjs'), `import fs from 'node:fs';\nfs.appendFileSync(process.env.LEGION_TEST_EVENTS, 'verify-desktop-artifacts ' + process.argv[2] + '\\n');\n`);
  write(path.join(repo, 'scripts', 'package-agent.mjs'), `import fs from 'node:fs';
fs.appendFileSync(process.env.LEGION_TEST_EVENTS, 'package-agent\\n');
const { version } = JSON.parse(fs.readFileSync(new URL('../agent/package.json', import.meta.url), 'utf8'));
const dist = new URL('../dist/', import.meta.url);
fs.writeFileSync(new URL('legionctl-agent-' + version + '.tgz', dist), 'the agent');
fs.writeFileSync(new URL('Legion-Control-agent-manifest.json', dist), JSON.stringify({ schema: 1, agentVersion: version }) + '\\n');
fs.writeFileSync(new URL('Legion-Control-agent-manifest.json.sig', dist), 'signature\\n');
`);
  write(path.join(repo, 'mac', 'build.sh'), '#!/bin/sh\n' + note('mac/build.sh $*') + `cd "$(dirname "$0")"
mkdir -p "build/Legion Control.app/Contents"
cp Resources/Info.plist "build/Legion Control.app/Contents/Info.plist"
`, 0o755);
  // What aapt reads out of the APK is what the Gradle file said when it was built.
  write(path.join(repo, 'android', 'build-apk.sh'), '#!/bin/sh\n' + note('android/build-apk.sh $*') + `cd "$(dirname "$0")"
name="$(sed -n 's/.*versionName = "\\(.*\\)".*/\\1/p' app/build.gradle.kts)"
code="$(sed -n 's/.*versionCode = \\([0-9]*\\).*/\\1/p' app/build.gradle.kts)"
mkdir -p app/build/outputs/apk/release
printf "package: name='com.x1f4r.legioncontrol' versionCode='%s' versionName='%s'\\n" "$code" "$name" > app/build/outputs/apk/release/app-release.apk
`, 0o755);
  write(path.join(repo, 'desktop', 'build.sh'), '#!/bin/sh\n' + note('desktop/build.sh $*') + `cd "$(dirname "$0")/../dist"
for archive in Legion-Control-linux-x64.tar.gz Legion-Control-linux-arm64.tar.gz Legion-Control-windows-x64.zip; do printf '%s' "$archive" > "$archive"; done
`, 0o755);
  write(path.join(root, 'sdk', 'build-tools', '35.0.0', 'aapt2'), '#!/bin/sh\ncat "$3"\n', 0o755);
  write(path.join(root, 'bin', 'gh'), GH, 0o755);
  write(path.join(repo, '.gitignore'), 'dist/\nmac/build/\nandroid/app/build/\n');
  const home = path.join(root, 'home');
  createSigningKey(path.join(home, '.legion-control', 'release-signing.key'), path.join(repo, 'contract', 'release-public-key.pem'));
  fs.writeFileSync(path.join(root, 'gitconfig'), '');

  const env = {
    ...process.env,
    PATH: `${path.join(root, 'bin')}:${process.env.PATH}`,
    HOME: home,
    ANDROID_SDK_ROOT: path.join(root, 'sdk'),
    GIT_CONFIG_GLOBAL: path.join(root, 'gitconfig'),
    GIT_CONFIG_NOSYSTEM: '1',
    GIT_AUTHOR_NAME: 'me', GIT_AUTHOR_EMAIL: 'me@example.invalid', GIT_COMMITTER_NAME: 'me', GIT_COMMITTER_EMAIL: 'me@example.invalid',
    LEGION_TEST_EVENTS: events,
    LEGION_TEST_GH: ghState,
    LEGION_TEST_ORIGIN: origin,
  };
  delete env.ANDROID_HOME;
  const gitIn = (directory) => (...args) => {
    const result = spawnSync('git', ['-C', directory, ...args], { encoding: 'utf8', env });
    assert.equal(result.status, 0, `git ${args.join(' ')}: ${result.stderr}`);
    return result.stdout.trim();
  };
  const git = gitIn(repo);
  const originGit = gitIn(origin);
  git('init', '-q', '-b', 'main');
  git('add', '-A');
  git('commit', '-qm', 'Base');
  git('tag', '-a', 'v1.3.4', '-m', 'Legion Control 1.3.4');
  fs.mkdirSync(origin);
  originGit('init', '-q', '--bare', '-b', 'main');
  git('remote', 'add', 'origin', origin);
  git('push', '-q', 'origin', 'refs/heads/main', 'refs/tags/v1.3.4');
  write(path.join(origin, 'hooks', 'post-receive'), `#!/bin/sh\nwhile read -r old new ref; do printf 'push %s\\n' "$ref" >> "${events}"; done\n`, 0o755);

  const gh = {
    get: () => JSON.parse(fs.readFileSync(ghState, 'utf8')),
    set: (change) => fs.writeFileSync(ghState, JSON.stringify(change(gh.get()), null, 2)),
    passed: (sha) => ({ workflowName: 'Checks', headSha: sha, status: 'completed', conclusion: 'success', createdAt: '2026-10-01T10:00:00Z' }),
  };
  fs.writeFileSync(ghState, JSON.stringify({
    releases: [{ tagName: 'v1.3.4', isDraft: false, isPrerelease: false, assets: [], url: 'https://github.com/x1f4r/legion-control/releases/tag/v1.3.4' }],
    runs: [gh.passed(git('rev-parse', 'HEAD'))],
    fail: {},
  }));
  fs.writeFileSync(path.join(root, 'notes.md'), 'The first paragraph both apps show.\n');

  const release = (...args) => {
    fs.writeFileSync(events, '');
    return spawnSync('bash', [path.join(repo, 'scripts', 'release.sh'), ...args], { cwd: root, encoding: 'utf8', env });
  };
  const happened = () => fs.readFileSync(events, 'utf8').trim().split('\n').filter(Boolean);
  const manifest = () => verifyManifest(path.join(repo, 'dist', 'Legion-Control-manifest.json'), path.join(repo, 'dist', 'Legion-Control-manifest.json.sig'), path.join(repo, 'contract', 'release-public-key.pem'));
  // Someone else's commit, made in a clone of their own and pushed to origin's main.
  const pushFromElsewhere = (subject) => {
    const elsewhere = fs.mkdtempSync(path.join(root, 'elsewhere '));
    const clone = spawnSync('git', ['clone', '-q', origin, elsewhere], { encoding: 'utf8', env });
    assert.equal(clone.status, 0, clone.stderr);
    const elsewhereGit = gitIn(elsewhere);
    fs.writeFileSync(path.join(elsewhere, 'elsewhere.txt'), subject + '\n');
    elsewhereGit('add', 'elsewhere.txt');
    elsewhereGit('commit', '-qm', subject);
    elsewhereGit('push', '-q', 'origin', 'HEAD:refs/heads/main');
    return elsewhereGit('rev-parse', 'HEAD');
  };
  return { root, repo, git, originGit, gh, release, happened, manifest, pushFromElsewhere, notes: path.join(root, 'notes.md') };
}

const BUILDS = ['check.sh all', 'package-agent', 'mac/build.sh --no-install --require-agent', 'android/build-apk.sh --require-agent', 'desktop/build.sh --require-agent', 'verify-desktop-artifacts 1.4.0'];
const ASSETS = ['Legion-Control-macos-arm64.zip', 'Legion-Control-android-arm64.apk', 'Legion-Control-linux-x64.tar.gz', 'Legion-Control-linux-arm64.tar.gz', 'Legion-Control-windows-x64.zip', 'legionctl-agent-3.1.0.tgz', 'Legion-Control-manifest.json', 'Legion-Control-manifest.json.sig', 'Legion-Control-agent-manifest.json', 'Legion-Control-agent-manifest.json.sig'];

test('a release puts every asset on a draft, then pushes main and the tag together, then publishes', mac, (t) => {
  const { repo, git, originGit, gh, release, happened, manifest, notes } = releaseCheckout(t);
  const base = git('rev-parse', 'HEAD');
  const result = release('1.4.0', '--notes', notes);
  assert.equal(result.status, 0, result.stderr);
  assert.match(result.stdout, /CI passed on [0-9a-f]{12} \(Checks\)\./);
  assert.match(result.stdout, /The newest version out is 1\.3\.4\./);
  assert.match(result.stdout, /Legion Control 1\.4\.0 is out\.\nhttps:\/\/github\.com\/x1f4r\/legion-control\/releases\/tag\/v1\.4\.0/);
  assert.deepEqual(happened(), [
    ...BUILDS,
    'gh release create v1.4.0 draft=true target=main assets=10',
    'push refs/heads/main',
    'push refs/tags/v1.4.0',
    'gh release edit v1.4.0 draft=false',
  ]);

  // One commit, the bump, and the tag on it; origin has both.
  const head = git('rev-parse', 'HEAD');
  assert.equal(git('log', '-1', '--format=%s'), 'Release 1.4.0');
  assert.equal(git('rev-parse', 'HEAD^'), base);
  assert.equal(originGit('rev-parse', 'refs/heads/main'), head);
  assert.equal(originGit('cat-file', '-t', 'refs/tags/v1.4.0'), 'tag');
  assert.equal(originGit('rev-parse', 'refs/tags/v1.4.0^{commit}'), head);
  assert.equal(git('status', '--porcelain'), '');

  // The signed manifest names that commit.
  const signed = manifest();
  assert.equal(signed.version, '1.4.0');
  assert.equal(signed.commit, head);
  const published = gh.get().releases.find(entry => entry.tagName === 'v1.4.0');
  assert.equal(published.isDraft, false);
  assert.equal(published.notes, 'The first paragraph both apps show.\n');
  assert.deepEqual(published.assets.map(asset => asset.name).sort(), [...ASSETS].sort());
  assert.equal(fs.existsSync(path.join(repo, 'dist', 'release-notes.md')), true);
});

test('a release that stopped after it was tagged is finished by --resume and by nothing else', mac, (t) => {
  const { git, originGit, gh, release, happened, manifest, notes } = releaseCheckout(t);
  const base = git('rev-parse', 'HEAD');

  // The upload drops half way: a draft with three assets, nothing pushed.
  gh.set(state => ({ ...state, fail: { create: 'partial' } }));
  const stopped = release('1.4.0', '--notes', notes);
  assert.equal(stopped.status, 1);
  assert.match(stopped.stderr, /v1\.4\.0 is tagged here and not out yet\. .*\n {4}\.\/scripts\/release\.sh 1\.4\.0 --resume/);
  assert.deepEqual(happened(), [...BUILDS, 'gh release create v1.4.0 draft=true target=main assets=10']);
  const tagged = git('rev-parse', 'refs/tags/v1.4.0^{commit}');
  assert.equal(originGit('rev-parse', 'refs/heads/main'), base);
  assert.equal(originGit('tag', '-l', 'v1.4.0'), '');

  // Starting it over is refused: the version is taken, and the way on is --resume.
  const again = release('1.4.0', '--notes', notes);
  assert.equal(again.status, 1);
  assert.match(again.stderr, /tag v1\.4\.0 already exists\. .*finish it with --resume\./);
  assert.deepEqual(happened(), []);

  // --resume builds nothing, fills the draft, pushes and publishes.
  gh.set(state => ({ ...state, fail: {} }));
  const resumed = release('1.4.0', '--resume');
  assert.equal(resumed.status, 0, resumed.stderr);
  assert.match(resumed.stdout, /v1\.4\.0 is at [0-9a-f]{40}, and GitHub has a draft of it\./);
  assert.deepEqual(happened(), [
    'verify-desktop-artifacts 1.4.0',
    'gh release upload v1.4.0 clobber=true assets=10',
    'push refs/heads/main',
    'push refs/tags/v1.4.0',
    'gh release edit v1.4.0 draft=false',
  ]);
  assert.equal(originGit('rev-parse', 'refs/tags/v1.4.0^{commit}'), tagged);
  assert.equal(manifest().commit, tagged);
  assert.equal(gh.get().releases.find(entry => entry.tagName === 'v1.4.0').isDraft, false);

  // Once it is out, there is nothing left to do.
  const done = release('1.4.0', '--resume');
  assert.equal(done.status, 0, done.stderr);
  assert.match(done.stdout, /v1\.4\.0 is already out: https:/);
  assert.deepEqual(happened(), []);
});

test('--resume creates the draft from the kept notes when there was none, and only publishes when that is all that is left', mac, (t) => {
  const { git, originGit, gh, release, happened, notes } = releaseCheckout(t);

  gh.set(state => ({ ...state, fail: { create: 'before' } }));
  assert.equal(release('1.4.0', '--notes', notes).status, 1);
  gh.set(state => ({ ...state, fail: { publish: true } }));
  const unpublished = release('1.4.0', '--resume');
  assert.equal(unpublished.status, 1);
  assert.match(unpublished.stderr, /--resume/);
  assert.deepEqual(happened(), [
    'verify-desktop-artifacts 1.4.0',
    'gh release create v1.4.0 draft=true target=main assets=10',
    'push refs/heads/main',
    'push refs/tags/v1.4.0',
    'gh release edit v1.4.0 draft=false',
  ]);
  const draft = gh.get().releases.find(entry => entry.tagName === 'v1.4.0');
  assert.equal(draft.notes, 'The first paragraph both apps show.\n');
  assert.equal(draft.isDraft, true);
  assert.equal(originGit('rev-parse', 'refs/heads/main'), git('rev-parse', 'HEAD'));

  gh.set(state => ({ ...state, fail: {} }));
  const published = release('1.4.0', '--resume');
  assert.equal(published.status, 0, published.stderr);
  // Main and the tag are on origin already, so nothing is pushed and the hook hears nothing.
  assert.match(published.stdout, new RegExp(`origin has v1\\.4\\.0 already, and main has ${git('rev-parse', 'HEAD')}\\.`));
  assert.deepEqual(happened(), ['verify-desktop-artifacts 1.4.0', 'gh release edit v1.4.0 draft=false']);
  assert.equal(gh.get().releases.find(entry => entry.tagName === 'v1.4.0').isDraft, false);
});

test('once origin has the tag, --resume only publishes, from the tag, whatever main has done since', mac, (t) => {
  const { git, originGit, gh, release, happened, pushFromElsewhere, notes } = releaseCheckout(t);
  const neverDeleted = /delete the tag|git tag -d|gh release delete/;

  // Main and the tag go out together, and then publishing fails: a public tag, and a draft.
  gh.set(state => ({ ...state, fail: { publish: true } }));
  const stopped = release('1.4.0', '--notes', notes);
  assert.equal(stopped.status, 1);
  assert.match(stopped.stderr, /v1\.4\.0 is on origin and not published yet\. .*\n {4}\.\/scripts\/release\.sh 1\.4\.0 --resume/);
  const tagged = git('rev-parse', 'refs/tags/v1.4.0^{commit}');
  assert.equal(originGit('rev-parse', 'refs/tags/v1.4.0^{commit}'), tagged);

  // Main moves on before anyone gets back to it. Pushing main again would be refused now, and the
  // tag is out of reach of deleting, so only the publishing is tried again.
  const later = pushFromElsewhere('A commit after the release');
  const failedAgain = release('1.4.0', '--resume');
  assert.equal(failedAgain.status, 1);
  assert.deepEqual(happened(), ['verify-desktop-artifacts 1.4.0', 'gh release edit v1.4.0 draft=false']);
  assert.match(failedAgain.stderr, /v1\.4\.0 is on origin and not published yet\./);
  assert.doesNotMatch(failedAgain.stderr, neverDeleted);

  // With main pulled here as well, HEAD is not the release any more, and the way on is the tag.
  git('pull', '-q', '--ff-only', 'origin', 'main');
  const moved = release('1.4.0', '--resume');
  assert.equal(moved.status, 1);
  assert.match(moved.stderr, new RegExp(`HEAD is not ${tagged}, the commit v1\\.4\\.0 is at\\. origin has v1\\.4\\.0 already, .*git checkout v1\\.4\\.0, then run this again\\.`));
  assert.doesNotMatch(moved.stderr, neverDeleted);
  assert.deepEqual(happened(), []);

  gh.set(state => ({ ...state, fail: {} }));
  git('checkout', '-q', 'v1.4.0');
  const published = release('1.4.0', '--resume');
  assert.equal(published.status, 0, published.stderr);
  assert.deepEqual(happened(), ['verify-desktop-artifacts 1.4.0', 'gh release edit v1.4.0 draft=false']);
  assert.equal(gh.get().releases.find(entry => entry.tagName === 'v1.4.0').isDraft, false);
  assert.equal(originGit('rev-parse', 'refs/heads/main'), later);
  assert.equal(originGit('rev-parse', 'refs/tags/v1.4.0^{commit}'), tagged);
});

test('--resume says when dist/ no longer holds the release, and how to go on from there', mac, (t) => {
  // Nothing of it public yet: the tag and the bump commit go, and it is cut again.
  const unpushed = releaseCheckout(t);
  unpushed.gh.set(state => ({ ...state, fail: { create: 'before' } }));
  assert.equal(unpushed.release('1.4.0', '--notes', unpushed.notes).status, 1);
  unpushed.gh.set(state => ({ ...state, fail: {} }));
  fs.rmSync(path.join(unpushed.repo, 'dist'), { recursive: true });
  const cleared = unpushed.release('1.4.0', '--resume', '--notes', unpushed.notes);
  assert.equal(cleared.status, 1);
  assert.match(cleared.stderr, /dist\/ no longer holds Legion-Control-macos-arm64\.zip .*legionctl-agent-3\.1\.0\.tgz .*, built for v1\.4\.0, and --resume builds nothing\. Nothing of v1\.4\.0 is public yet, so cut it again: git tag -d v1\.4\.0, git reset --hard origin\/main to drop the bump commit, then \.\/scripts\/release\.sh 1\.4\.0\./);
  assert.doesNotMatch(cleared.stderr, /left no notes|ditto|is tagged here and not out yet/);
  assert.deepEqual(unpushed.happened(), []);
  // Which is a way that works.
  unpushed.git('tag', '-d', 'v1.4.0');
  unpushed.git('reset', '-q', '--hard', 'origin/main');
  const again = unpushed.release('1.4.0', '--notes', unpushed.notes);
  assert.equal(again.status, 0, again.stderr);
  assert.equal(unpushed.gh.get().releases.find(entry => entry.tagName === 'v1.4.0').isDraft, false);

  // Once origin has the tag, the version is taken, and what is gone has to be built again.
  const pushed = releaseCheckout(t);
  pushed.gh.set(state => ({ ...state, fail: { publish: true } }));
  assert.equal(pushed.release('1.4.0', '--notes', pushed.notes).status, 1);
  pushed.gh.set(state => ({ ...state, fail: {} }));
  fs.rmSync(path.join(pushed.repo, 'dist', 'Legion-Control-android-arm64.apk'));
  const lost = pushed.release('1.4.0', '--resume');
  assert.equal(lost.status, 1);
  assert.match(lost.stderr, /dist\/ no longer holds Legion-Control-android-arm64\.apk, built for v1\.4\.0, and --resume builds nothing\. origin has v1\.4\.0 already, so the version cannot be cut again: build them at v1\.4\.0 by hand into dist\//);
  assert.doesNotMatch(lost.stderr, /git tag -d|gh release delete/);
  assert.deepEqual(pushed.happened(), []);
});

test('a tag that got to origin on its own is not published until origin\'s main has the commit it is at', mac, (t) => {
  // The run stops before its draft, and the tag is pushed by hand, without main.
  const tagPushedAlone = (checkout) => {
    checkout.gh.set(state => ({ ...state, fail: { create: 'before' } }));
    assert.equal(checkout.release('1.4.0', '--notes', checkout.notes).status, 1);
    checkout.gh.set(state => ({ ...state, fail: {} }));
    checkout.git('push', '-q', 'origin', 'refs/tags/v1.4.0');
    return checkout.git('rev-parse', 'refs/tags/v1.4.0^{commit}');
  };
  const draftOf = (checkout) => checkout.gh.get().releases.find(entry => entry.tagName === 'v1.4.0');

  // Main one commit short of it, the bump: main follows the tag once the draft is filled, and only
  // then is the release published.
  const behind = releaseCheckout(t);
  const base = behind.git('rev-parse', 'HEAD');
  const tagged = tagPushedAlone(behind);
  assert.equal(behind.originGit('rev-parse', 'refs/heads/main'), base);
  const resumed = behind.release('1.4.0', '--resume');
  assert.equal(resumed.status, 0, resumed.stderr);
  assert.match(resumed.stdout, /origin has v1\.4\.0, and its main is one commit short of it: main follows once the draft holds every asset\./);
  assert.match(resumed.stdout, new RegExp(`origin had v1\\.4\\.0 already, and main has ${tagged} now as well\\.`));
  assert.deepEqual(behind.happened(), [
    'verify-desktop-artifacts 1.4.0',
    'gh release create v1.4.0 draft=true target=main assets=10',
    'push refs/heads/main',
    'gh release edit v1.4.0 draft=false',
  ]);
  assert.equal(behind.originGit('rev-parse', 'refs/heads/main'), tagged);
  assert.equal(draftOf(behind).isDraft, false);

  // Main moved on without it: nothing is published until the tag is merged into it, and the way the
  // refusal gives is one that works.
  const movedOn = releaseCheckout(t);
  const movedOnTag = tagPushedAlone(movedOn);
  movedOn.pushFromElsewhere('A commit pushed from elsewhere');
  const refused = movedOn.release('1.4.0', '--resume');
  assert.equal(refused.status, 1);
  assert.match(refused.stderr, new RegExp(`origin has v1\\.4\\.0, but its main does not have ${movedOnTag}, the commit v1\\.4\\.0 is at, and has moved on to commits v1\\.4\\.0 does not have\\. Merge the tag into origin's main and push that \\(git checkout --detach origin/main, git merge v1\\.4\\.0, git push origin HEAD:refs/heads/main\\), then run this again from the tag: git checkout v1\\.4\\.0\\.`));
  assert.doesNotMatch(refused.stderr, /is on origin and not published yet|and main has/);
  assert.deepEqual(movedOn.happened(), []);
  assert.equal(draftOf(movedOn), undefined);
  movedOn.git('checkout', '-q', '--detach', 'origin/main');
  movedOn.git('merge', '-q', '--no-edit', 'v1.4.0');
  movedOn.git('push', '-q', 'origin', 'HEAD:refs/heads/main');
  movedOn.git('checkout', '-q', 'v1.4.0');
  const merged = movedOn.release('1.4.0', '--resume');
  assert.equal(merged.status, 0, merged.stderr);
  assert.match(merged.stdout, new RegExp(`origin has v1\\.4\\.0 already, and main has ${movedOnTag}\\.`));
  assert.deepEqual(movedOn.happened(), [
    'verify-desktop-artifacts 1.4.0',
    'gh release create v1.4.0 draft=true target=main assets=10',
    'gh release edit v1.4.0 draft=false',
  ]);
  assert.equal(draftOf(movedOn).isDraft, false);

  // More than the bump short of it is more than a release pushes: it is refused, with the push that
  // would put it right if it is meant to.
  const farBehind = releaseCheckout(t);
  const before = farBehind.git('rev-parse', 'HEAD');
  fs.writeFileSync(path.join(farBehind.repo, 'reviewed.txt'), 'a reviewed commit\n');
  farBehind.git('add', 'reviewed.txt');
  farBehind.git('commit', '-qm', 'Reviewed');
  farBehind.git('push', '-q', 'origin', 'main');
  farBehind.gh.set(state => ({ ...state, runs: [farBehind.gh.passed(farBehind.git('rev-parse', 'HEAD'))] }));
  const farTag = tagPushedAlone(farBehind);
  farBehind.git('push', '-q', '--force', 'origin', `${before}:refs/heads/main`);
  const tooFar = farBehind.release('1.4.0', '--resume');
  assert.equal(tooFar.status, 1);
  assert.match(tooFar.stderr, new RegExp(`origin has v1\\.4\\.0, but its main is 2 commits behind ${farTag}, the commit v1\\.4\\.0 is at, and a release pushes one commit at most, its version bump\\. .*git push origin ${farTag}:refs/heads/main, then run this again\\.`));
  assert.deepEqual(farBehind.happened(), []);
  assert.equal(farBehind.originGit('rev-parse', 'refs/heads/main'), before);
});

test('--resume refuses what it cannot finish, without sending anyone round to --resume again', mac, (t) => {
  const { repo, git, originGit, release, happened } = releaseCheckout(t);
  const refused = (args, message) => {
    const result = release(...args);
    assert.equal(result.status, 1, result.stdout);
    assert.match(result.stderr, message);
    assert.doesNotMatch(result.stderr, /is tagged here and not out yet|is on origin and not published yet/);
    assert.deepEqual(happened(), []);
    return result;
  };
  refused(['1.4.0', '--resume'], /there is no tag v1\.4\.0 here, so there is nothing to resume\./);
  refused(['1.4.0', '--resume', '--dry-run'], /--resume finishes v1\.4\.0 the way it was started, so it takes neither --dry-run nor --tag-head\./);
  git('tag', '-a', 'v1.4.0', '-m', 'Legion Control 1.4.0');
  refused(['1.4.0', '--resume'], /v1\.4\.0 is at a commit whose projects say 1\.3\.4, not 1\.4\.0\./);
  const tagged = git('rev-parse', 'HEAD');
  fs.writeFileSync(path.join(repo, 'later.txt'), 'a commit after the tag\n');
  git('add', 'later.txt');
  git('commit', '-qm', 'Later');
  // Only a tag origin does not have yet can still be deleted.
  refused(['1.4.0', '--resume'], new RegExp(`HEAD is not ${tagged}, the commit v1\\.4\\.0 is at\\. Go back to it, or delete the tag`));

  // A v1.4.0 on origin that is another tag object is not this release's, whatever it points at.
  originGit('tag', 'v1.4.0', tagged);
  refused(['1.4.0', '--resume'], /origin has a tag v1\.4\.0 that is not the one here\. Find out where it came from before anything is published\./);
  originGit('tag', '-d', 'v1.4.0');

  // This very tag on origin is public, and the way on is the tag, never deleting it.
  git('push', '-q', 'origin', 'refs/tags/v1.4.0');
  const pushed = refused(['1.4.0', '--resume'], new RegExp(`HEAD is not ${tagged}, the commit v1\\.4\\.0 is at\\. origin has v1\\.4\\.0 already, so the version is taken and the release can only be finished, from the tag: git checkout v1\\.4\\.0, then run this again\\.`));
  assert.doesNotMatch(pushed.stderr, /delete/);
});

test('a release is refused before anything is built unless CI passed on the very commit origin has, and the version is new', mac, (t) => {
  const { repo, git, originGit, gh, release, happened, pushFromElsewhere, notes } = releaseCheckout(t);
  const base = git('rev-parse', 'HEAD');
  const refused = (args, message) => {
    const result = release(...args);
    assert.equal(result.status, 1, result.stdout);
    assert.match(result.stderr, message);
    assert.deepEqual(happened(), [], `${args.join(' ')} built or published something`);
    assert.equal(git('status', '--porcelain'), '');
    assert.equal(git('rev-parse', 'HEAD'), base);
    assert.equal(git('tag', '-l', 'v1.4.0'), '');
  };
  const runs = (...list) => gh.set(state => ({ ...state, runs: list }));

  runs({ ...gh.passed(base), conclusion: 'failure' });
  refused(['1.4.0', '--notes', notes], /CI did not pass on [0-9a-f]{12} \(Checks: failure\)/);
  runs({ ...gh.passed(base), status: 'in_progress', conclusion: '' });
  refused(['1.4.0', '--notes', notes], /CI is still running on [0-9a-f]{12} \(Checks\)/);
  runs();
  refused(['1.4.0', '--notes', notes], /CI has not run on [0-9a-f]{12}/);
  runs(gh.passed(base));

  refused(['1.3.3', '--notes', notes], /1\.3\.3 is not above 1\.3\.4, the newest version already released\./);
  refused(['1.3.4', '--notes', notes], /tag v1\.3\.4 already exists\. Versions are not reused/);
  refused(['1.4.0', '--tag-head', '--notes', notes], /--tag-head releases HEAD as it is, and its projects say 1\.3\.4, not 1\.4\.0/);

  gh.set(state => ({ ...state, releases: [...state.releases, { tagName: 'v1.4.0', isDraft: true, isPrerelease: false, assets: [] }] }));
  refused(['1.4.0', '--notes', notes], /GitHub has a draft release v1\.4\.0, left by a run that stopped\. Finish that run with --resume/);
  gh.set(state => ({ ...state, releases: state.releases.filter(entry => entry.tagName !== 'v1.4.0') }));

  // A tag on origin claims its version before any release is made of it.
  originGit('tag', 'v1.4.0', base);
  refused(['1.4.0', '--notes', notes], /tag v1\.4\.0 is already on origin\. Versions are not reused\./);
  originGit('tag', '-d', 'v1.4.0');

  // A build number is only known to be above the last release's if that one can be read.
  const bare = originGit('commit-tree', '4b825dc642cb6eb9a060e54bf8d69288fbee4904', '-m', 'Before there was an Android app');
  originGit('tag', 'v1.3.5', bare);
  gh.set(state => ({ ...state, releases: [...state.releases, { tagName: 'v1.3.5', isDraft: false, isPrerelease: false, assets: [] }] }));
  refused(['1.4.0', '--notes', notes], /cannot read the build number v1\.3\.5 went out with, so there is no telling whether 15 comes after it\./);
  gh.set(state => ({ ...state, releases: state.releases.filter(entry => entry.tagName !== 'v1.3.5') }));
  originGit('tag', '-d', 'v1.3.5');

  // HEAD behind origin is not the commit a release would go out as.
  pushFromElsewhere('A commit pushed from elsewhere');
  refused(['1.4.0', '--notes', notes], /HEAD is 0 commit\(s\) ahead of origin\/main and 1 behind it\./);
  git('push', '-q', '--force', 'origin', `${base}:refs/heads/main`);

  // A commit origin does not have yet is a commit CI has not checked.
  fs.writeFileSync(path.join(repo, 'unpushed.txt'), 'not on origin\n');
  git('add', 'unpushed.txt');
  git('commit', '-qm', 'Unpushed');
  const ahead = release('1.4.0', '--notes', notes);
  assert.equal(ahead.status, 1);
  assert.match(ahead.stderr, /HEAD is 1 commit\(s\) ahead of origin\/main and 0 behind it\./);
  assert.deepEqual(happened(), []);
});

test('--tag-head releases the reviewed HEAD without a bump commit, its build above the last release', mac, (t) => {
  const { repo, git, originGit, gh, release, happened, manifest, notes } = releaseCheckout(t);
  // The versions set in a commit of their own, reviewed and merged like any other.
  const bump = (build) => {
    edit(repo, 'mac/Resources/Info.plist', '<string>1.3.4</string>', '<string>1.4.0</string>');
    edit(repo, 'mac/Resources/Info.plist', '<string>14</string>', `<string>${build}</string>`);
    edit(repo, 'android/app/build.gradle.kts', 'versionCode = 14', `versionCode = ${build}`);
    edit(repo, 'android/app/build.gradle.kts', 'versionName = "1.3.4"', 'versionName = "1.4.0"');
    edit(repo, 'desktop/LegionControl.Desktop/LegionControl.Desktop.csproj', '<Version>1.3.4</Version>', '<Version>1.4.0</Version>');
    git('commit', '-qam', 'Set the versions to 1.4.0');
    git('push', '-q', 'origin', 'main');
    gh.set(state => ({ ...state, runs: [gh.passed(git('rev-parse', 'HEAD'))] }));
  };

  // A build number left where the last release had it is refused before anything is built.
  bump(14);
  const same = release('1.4.0', '--tag-head', '--notes', notes);
  assert.equal(same.status, 1);
  assert.match(same.stderr, /build 14 is not above 14, the build v1\.3\.4 went out with\./);
  assert.deepEqual(happened(), []);

  git('reset', '-q', '--hard', 'HEAD^');
  git('push', '-q', '--force', 'origin', 'main');
  bump(15);
  const reviewed = git('rev-parse', 'HEAD');
  // Without --tag-head the bump would have nothing to change.
  const bumped = release('1.4.0', '--notes', notes);
  assert.equal(bumped.status, 1);
  assert.match(bumped.stderr, /the projects already say 1\.4\.0\. To release HEAD as it is, without a commit that bumps nothing, use --tag-head\./);
  assert.deepEqual(happened(), []);

  const result = release('1.4.0', '--tag-head', '--notes', notes);
  assert.equal(result.status, 0, result.stderr);
  assert.match(result.stdout, /Releasing HEAD as it is: 1\.4\.0 \(15\)\./);
  assert.deepEqual(happened(), [
    ...BUILDS,
    'gh release create v1.4.0 draft=true target=main assets=10',
    'push refs/tags/v1.4.0',
    'gh release edit v1.4.0 draft=false',
  ]);
  assert.equal(git('rev-parse', 'HEAD'), reviewed);
  assert.equal(originGit('rev-parse', 'refs/tags/v1.4.0^{commit}'), reviewed);
  assert.equal(manifest().commit, reviewed);
});

test('a dry run builds and signs, says what a real run would do and would stop at, and leaves nothing behind', mac, (t) => {
  const { git, originGit, gh, release, happened, manifest, notes } = releaseCheckout(t);
  const base = git('rev-parse', 'HEAD');
  gh.set(state => ({ ...state, runs: [{ ...gh.passed(base), conclusion: 'failure' }] }));
  const result = release('1.4.0', '--dry-run', '--notes', notes);
  assert.equal(result.status, 0, result.stderr);
  assert.match(result.stderr, /A real run would stop here: CI did not pass on/);
  assert.deepEqual(happened(), BUILDS);
  assert.match(result.stdout, /From here a real run would:\n {4}commit the bump as "Release 1\.4\.0", tag it v1\.4\.0 and sign the manifest naming it\n {4}upload all ten assets to a draft release v1\.4\.0\n {4}push main and v1\.4\.0 to origin together, then publish the draft\n/);
  assert.match(result.stdout, /It would not get this far, though/);
  assert.doesNotMatch(result.stdout, /gh release create|git push/);
  // Built from a bump that is put back, so the manifest names no commit rather than a wrong one.
  const signed = manifest();
  assert.equal(signed.version, '1.4.0');
  assert.equal('commit' in signed, false);
  assert.equal(git('status', '--porcelain'), '');
  assert.equal(git('rev-parse', 'HEAD'), base);
  assert.equal(git('tag', '-l', 'v1.4.0'), '');
  assert.equal(originGit('rev-parse', 'refs/heads/main'), base);
});
