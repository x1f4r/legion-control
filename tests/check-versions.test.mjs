import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import test from 'node:test';
import { AGENT_COPIES, BUNDLE_LOOKUPS, agentFloorProblem, checkVersions, compareVersions } from '../scripts/check-versions.mjs';
import { edit, versionTree } from './fixtures/version-tree.mjs';

const repository = path.resolve(import.meta.dirname, '..');
const script = path.join(repository, 'scripts', 'check-versions.mjs');

// A fresh tree for every check: the check imports the tree's config.mjs, and Node hands back the
// module it already loaded for a path it has seen.
function tree(t, change = () => {}, options) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'legion-versions-'));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  versionTree(root, options);
  change(root);
  return root;
}
const check = async (t, change, options) => (await checkVersions(tree(t, change, options))).problems;
const files = (problems) => problems.map(problem => problem.file);

test('versions in step pass, and the command says which ones it checked', async (t) => {
  const root = tree(t);
  const result = await checkVersions(root);
  assert.deepEqual(result.problems, []);
  assert.deepEqual(result.agent, { version: '3.1.0', floor: '3.1.0' });
  assert.deepEqual(result.app, { version: '1.3.4', build: '14' });
  const run = spawnSync(process.execPath, [script, root], { encoding: 'utf8' });
  assert.equal(run.status, 0, run.stderr);
  assert.equal(run.stdout, 'Versions agree: agent 3.1.0 (floor 3.1.0), app 1.3.4 (build 14).\n');
});

test('a client that would offer another agent than agent/package.json is named', async (t) => {
  for (const copy of AGENT_COPIES) {
    const problems = await check(t, root => edit(root, copy.file, '"3.1.0"', '"3.0.1"'));
    assert.deepEqual(files(problems), [copy.file]);
    assert.match(problems[0].message, new RegExp(`${copy.name} = "3\\.0\\.1", but agent/package\\.json says 3\\.1\\.0`));
  }
  const run = spawnSync(process.execPath, [script, tree(t, root => edit(root, AGENT_COPIES[0].file, '"3.1.0"', '"3.0.1"'))], { encoding: 'utf8' });
  assert.equal(run.status, 1);
  assert.equal(run.stdout, '');
  assert.match(run.stderr, /bundledAgentVersion = "3\.0\.1"/);
});

test('the agent takes its version from agent/package.json rather than a copy of its own', async (t) => {
  const config = 'agent/src/config.mjs';
  const literal = (version) => (root) => fs.writeFileSync(path.join(root, config), `export const AGENT_VERSION = '${version}';\n`);
  // Agreeing today is not enough: a second copy is the one that is forgotten at the next bump.
  const same = await check(t, literal('3.1.0'));
  assert.deepEqual(files(same), [config]);
  assert.match(same[0].message, /literal/);
  const stale = await check(t, literal('3.0.1'));
  assert.deepEqual(files(stale), [config, config]);
  assert.match(stale[1].message, /reports agent "3\.0\.1", but agent\/package\.json says 3\.1\.0/);
  // config.mjs split up, passing on a version written out in a module of its own.
  const moved = await check(t, (root) => {
    fs.mkdirSync(path.join(root, 'agent/src/config'));
    fs.writeFileSync(path.join(root, 'agent/src/config/version.mjs'), "export const AGENT_VERSION = '3.1.0';\n");
    fs.writeFileSync(path.join(root, config), "export { AGENT_VERSION } from './config/version.mjs';\n");
  });
  assert.deepEqual(files(moved), ['agent/src/config/version.mjs']);
  assert.match(moved[0].message, /agent\/src\/config\/version\.mjs writes AGENT_VERSION out as a literal/);
});

test('the Android build bundles the agent of agent/package.json when nobody says otherwise', async (t) => {
  const gradle = 'android/app/build.gradle.kts';
  const stale = await check(t, root => edit(root, gradle, 'getOrElse("3.1.0")', 'getOrElse("3.0.1")'));
  assert.deepEqual(files(stale), [gradle]);
  assert.match(stale[0].message, /bundles agent 3\.0\.1 unless told otherwise/);
  // Reading the version out of agent/package.json is what the default should do in the end.
  const reading = await check(t, root => edit(root, gradle, 'providers.gradleProperty("legion.agentVersion").getOrElse("3.1.0")',
    'providers.gradleProperty("legion.agentVersion").getOrElse(agentPackageVersion(rootProject.file("../agent/package.json")))'));
  assert.deepEqual(reading, []);
  // The way the Android build reads it: no default of its own, only the version in the file.
  const fromFile = await check(t, root => edit(root, gradle, '.getOrElse("3.1.0")', `
        .orElse(
            providers.fileContents(rootProject.layout.projectDirectory.file("../agent/package.json")).asText.map { text ->
                (groovy.json.JsonSlurper().parseText(text) as Map<*, *>)["version"] as? String
                    ?: throw GradleException("agent/package.json does not say which version the agent is.")
            },
        )
        .get()`));
  assert.deepEqual(fromFile, []);
  const neither = await check(t, root => edit(root, gradle, '.getOrElse("3.1.0")', '.get()'));
  assert.deepEqual(neither.map(({ file, missing }) => ({ file, missing })), [{ file: gradle, missing: true }]);
  // Saying so in a comment is not reading it: the comment is what stays behind when the read goes.
  for (const comment of [
    '    // The default comes from agent/package.json.\n',
    '    /** Bundles the agent "../agent/package.json" names. */\n',
    '    /**\n     * Bundles the agent "agent/package.json" names.\n     */\n',
  ]) {
    const said = await check(t, root => edit(root, gradle, '    val version = providers.gradleProperty("legion.agentVersion").getOrElse("3.1.0")',
      `${comment}    val version = providers.gradleProperty("legion.agentVersion").get()`));
    assert.deepEqual(said.map(({ file, missing }) => ({ file, missing })), [{ file: gradle, missing: true }], comment);
  }
  // Nor is a message that mentions the file, or a path to another package.json.
  const mentioned = await check(t, root => edit(root, gradle, '.getOrElse("3.1.0")',
    '.orElse(providers.fileContents(rootProject.layout.projectDirectory.file("../desktop/package.json")).asText.map { error("agent/package.json is not read here") }).get()'));
  assert.deepEqual(mentioned.map(({ file, missing }) => ({ file, missing })), [{ file: gradle, missing: true }]);
});

test('the version floor is declared, x.y.z and at most the agent version', async (t) => {
  assert.match(agentFloorProblem({ version: '3.1.0' }), /declares no legionControl\.agentFloor/);
  assert.match(agentFloorProblem({ version: '3.1.0', legionControl: { agentFloor: '3.1' } }), /"3\.1", which is not an x\.y\.z version/);
  assert.match(agentFloorProblem({ version: '3.1.0', legionControl: { agentFloor: 310 } }), /310, which is not an x\.y\.z version/);
  assert.match(agentFloorProblem({ version: '3.1.0', legionControl: { agentFloor: '3.2.0' } }), /3\.2\.0, above its own version 3\.1\.0/);
  // Compared as numbers: 3.10.0 is above 3.9.0 even though it sorts below it as text.
  assert.match(agentFloorProblem({ version: '3.9.0', legionControl: { agentFloor: '3.10.0' } }), /above its own version/);
  assert.equal(agentFloorProblem({ version: '3.10.0', legionControl: { agentFloor: '3.9.0' } }), null);
  assert.equal(agentFloorProblem({ version: '3.1.0', legionControl: { agentFloor: '3.1.0' } }), null);
  assert.deepEqual([compareVersions('3.1.0', '3.1.0'), compareVersions('3.0.9', '3.1.0'), compareVersions('4.0.0', '3.99.99')], [0, -1, 1]);

  assert.deepEqual(files(await check(t, undefined, { floor: null })), ['agent/package.json']);
  assert.deepEqual(files(await check(t, undefined, { floor: '3.2.0' })), ['agent/package.json']);
  assert.deepEqual(await check(t, undefined, { floor: '3.0.0' }), []);
});

test('a build that looks for the agent tarball under a written-out version is named', async (t) => {
  // The way android/build-apk.sh used to look for it, which found nothing once the agent moved on.
  const problems = await check(t, root => edit(root, 'android/build-apk.sh', '"legionctl-agent-$AGENT_VERSION.tgz"', 'legionctl-agent-3.1.0.tgz'));
  assert.deepEqual(files(problems), ['android/build-apk.sh']);
  assert.match(problems[0].message, /looks for legionctl-agent-3\.1\.0\.tgz by name/);
  const desktop = await check(t, root => edit(root, 'desktop/LegionControl.Desktop/LegionControl.Desktop.csproj', 'legionctl-agent-*.tgz', 'legionctl-agent-3.0.1.tgz'));
  assert.deepEqual(files(desktop), ['desktop/LegionControl.Desktop/LegionControl.Desktop.csproj']);
});

test('the fixture generator takes the version and the floor from agent/package.json', async (t) => {
  const generator = 'contract/tools/make-fixtures.mjs';
  const literal = await check(t, root => edit(root, generator, 'const AGENT = AGENT_PACKAGE.version;', "const AGENT = '3.1.0';"));
  assert.deepEqual(files(literal), [generator]);
  assert.match(literal[0].message, /agent version out as a literal/);
  const unread = await check(t, root => edit(root, generator, "path.join(HERE, '..', '..', 'agent', 'package.json')", "path.join(HERE, 'agent.json')"));
  assert.deepEqual(files(unread), [generator]);
  assert.match(unread[0].message, /does not read agent\/package\.json/);
  const floor = await check(t, root => edit(root, generator, 'AGENT_PACKAGE.legionControl.agentFloor', "'3.1.0'"));
  assert.deepEqual(files(floor), [generator]);
  assert.match(floor[0].message, /version floor/);
});

test('the three clients agree about the app version and the build number', async (t) => {
  const plist = 'mac/Resources/Info.plist';
  const gradle = 'android/app/build.gradle.kts';
  const desktop = 'desktop/LegionControl.Desktop/LegionControl.Desktop.csproj';
  const android = await check(t, root => edit(root, gradle, 'versionName = "1.3.4"', 'versionName = "1.4.0"'));
  assert.deepEqual(files(android), [gradle]);
  assert.match(android[0].message, /versionName 1\.4\.0, but mac\/Resources\/Info\.plist says 1\.3\.4/);
  assert.deepEqual(files(await check(t, root => edit(root, desktop, '<Version>1.3.4</Version>', '<Version>1.3.5</Version>'))), [desktop]);
  const build = await check(t, root => edit(root, plist, '<string>14</string>', '<string>15</string>'));
  assert.deepEqual(files(build), [gradle]);
  assert.match(build[0].message, /versionCode 14, but mac\/Resources\/Info\.plist says 15/);
  assert.match((await check(t, undefined, { app: '1.4' }))[0].message, /CFBundleShortVersionString "1\.4", which is not an x\.y\.z version/);
  assert.match((await check(t, undefined, { build: '14b' }))[0].message, /CFBundleVersion "14b", which is not a whole number/);
});

test('a copy that moved is reported rather than passed over', async (t) => {
  const renamed = await check(t, root => edit(root, AGENT_COPIES[0].file, 'let bundledAgentVersion', 'let shippedAgentVersion'));
  assert.deepEqual(renamed.map(({ file, missing }) => ({ file, missing })), [{ file: AGENT_COPIES[0].file, missing: true }]);
  const gone = await check(t, root => fs.rmSync(path.join(root, 'mac/Resources/Info.plist')));
  assert.deepEqual(gone.map(({ file, missing }) => ({ file, missing })), [{ file: 'mac/Resources/Info.plist', missing: true }]);
  assert.match(gone[0].message, /cannot be read \(ENOENT\)/);
  // The module the agent's version is asked of, gone with nothing in its place.
  const config = await check(t, root => fs.rmSync(path.join(root, 'agent/src/config.mjs')));
  assert.deepEqual(config.map(({ file, missing }) => ({ file, missing })), [{ file: 'agent/src/config.mjs', missing: true }]);
});

test('this repository has every copy where the check looks, and its builds follow agent/package.json', async () => {
  const { problems } = await checkVersions(repository);
  assert.deepEqual(problems.filter(problem => problem.missing), []);
  assert.deepEqual(problems.filter(problem => BUNDLE_LOOKUPS.includes(problem.file) || problem.file === 'contract/tools/make-fixtures.mjs'), []);
});
