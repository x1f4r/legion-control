#!/usr/bin/env node
// Check that every copy of a version number says what its source says.
//
//   node scripts/check-versions.mjs               this checkout
//   node scripts/check-versions.mjs <directory>   another one
//
// agent/package.json is the one place the agent version is decided. The agent reads its version
// from it, the contract fixtures are generated from it, and the builds find the signed bundle in
// dist/ by it. Each client still names in its own code the agent it carries a bundle of, because
// that is the agent it offers to install, so those copies have to say the same thing or a client
// offers an agent it does not have. The app version and build live in the three client projects,
// which release.sh bumps together and checks every artifact against.
//
// scripts/check.sh runs this, and release.sh runs it again after the bump.
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const repository = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const STABLE = /^\d+\.\d+\.\d+$/;

/** Orders two x.y.z versions number by number, the way the agent compares them (contract section 13). */
export function compareVersions(a, b) {
  const [x, y] = [a, b].map(version => version.split('.').map(Number));
  const index = x.findIndex((part, at) => part !== y[at]);
  return index < 0 ? 0 : Math.sign(x[index] - y[index]);
}

/**
 * Why the version floor in agent/package.json cannot ship, or null when it can.
 *
 * Every signed agent declares in `legionControl.agentFloor` the oldest agent it may be replaced
 * by, at most its own version (contract section 13). A release without one leaves the floor of
 * every machine it is installed on where it was. One above the version can never run: the agent
 * refuses such a bundle, and its launcher refuses a signed tree below the floor.
 */
export function agentFloorProblem(agentPackage) {
  const { version } = agentPackage ?? {};
  const floor = agentPackage?.legionControl?.agentFloor;
  if (floor === undefined) return 'agent/package.json declares no legionControl.agentFloor, the oldest agent that may replace this one (contract section 13).';
  if (typeof floor !== 'string' || !STABLE.test(floor)) return `agent/package.json declares legionControl.agentFloor ${JSON.stringify(floor)}, which is not an x.y.z version.`;
  if (STABLE.test(version ?? '') && compareVersions(floor, version) > 0) return `agent/package.json declares legionControl.agentFloor ${floor}, above its own version ${version}; an agent refuses a bundle whose floor is above itself.`;
  return null;
}

/** The agent each client build carries a signed bundle of, and offers to install. */
export const AGENT_COPIES = [
  { file: 'mac/Sources/LegionControlCore/AgentContract.swift', name: 'bundledAgentVersion', pattern: /\blet bundledAgentVersion = "([^"]*)"/ },
  { file: 'desktop/LegionControl.Desktop/Contract/AgentCapabilities.cs', name: 'BundledAgentVersion', pattern: /\bconst string BundledAgentVersion = "([^"]*)"/ },
  { file: 'android/app/src/main/java/com/x1f4r/legioncontrol/agent/Contract.kt', name: 'BUNDLED_AGENT_VERSION', pattern: /\bconst val BUNDLED_AGENT_VERSION = "([^"]*)"/ },
];

/**
 * Every module of the agent, relative to `root` and with forward slashes, in a stable order. None
 * when there is no agent/src, which the check reports when it reads config.mjs.
 */
function agentModules(root) {
  try {
    return fs.readdirSync(path.join(root, 'agent/src'), { recursive: true, withFileTypes: true })
      .filter(entry => entry.isFile() && /\.[cm]?js$/.test(entry.name))
      .map(entry => path.relative(root, path.join(entry.parentPath, entry.name)).split(path.sep).join('/'))
      .sort();
  } catch { return []; }
}

// The builds that look the signed bundle up in dist/ by its file name. A name with the version
// written into it goes stale with the next agent, and the build then either stops or ships
// without the agent, so each takes the version from agent/package.json or the bundle's manifest.
export const BUNDLE_LOOKUPS = [
  'android/build-apk.sh',
  'android/app/build.gradle.kts',
  'desktop/build.sh',
  'desktop/LegionControl.Desktop/LegionControl.Desktop.csproj',
  'mac/build.sh',
  'scripts/release.sh',
  'scripts/sign-release.mjs',
];

// The Android build copies the bundle named by legion.agentVersion into the APK. build-apk.sh
// passes it from agent/package.json; every other build (Android Studio, CI) gets the default, so
// the default either comes from agent/package.json as well or says the same version.
const GRADLE = 'android/app/build.gradle.kts';
const GRADLE_DEFAULT = /gradleProperty\("legion\.agentVersion"\)[^\n]*?getOrElse\("([^"]*)"\)/;
// A Gradle file that reads agent/package.json names it in a Kotlin string, in its code. A comment
// saying the default comes from agent/package.json is what stays behind when it no longer does, so
// whole comment lines, where Gradle files keep their prose, are passed over.
const GRADLE_READS_PACKAGE = /"[^"\n]*\bagent\/package\.json"/;
const gradleCode = (text) => text.split('\n').filter(line => !/^\s*(?:\/\/|\/\*|\*)/.test(line)).join('\n');

// The app version and build, as release.sh reads and bumps them. The Mac comes first because it
// is what release.sh reads the current version from; the desktop project has no build number.
const PLIST = 'mac/Resources/Info.plist';
const DESKTOP = 'desktop/LegionControl.Desktop/LegionControl.Desktop.csproj';
const APP_VERSIONS = [
  { file: PLIST, name: 'CFBundleShortVersionString', pattern: /<key>CFBundleShortVersionString<\/key>\s*<string>([^<]*)<\/string>/ },
  { file: GRADLE, name: 'versionName', pattern: /^[ \t]*versionName = "([^"]*)"/m },
  { file: DESKTOP, name: '<Version>', pattern: /<Version>([^<]*)<\/Version>/ },
];
const APP_BUILDS = [
  { file: PLIST, name: 'CFBundleVersion', pattern: /<key>CFBundleVersion<\/key>\s*<string>([^<]*)<\/string>/ },
  { file: GRADLE, name: 'versionCode', pattern: /^[ \t]*versionCode = (\S+?)\s*$/m },
];

/**
 * Every disagreement between a version and its copies, as `{ file, message }`.
 *
 * `missing` marks a copy this script could not find at all, which is a problem with the script as
 * much as with the tree: a declaration that moved has to be followed here, or it goes unchecked.
 */
export async function checkVersions(root = repository) {
  const problems = [];
  const problem = (file, message, extra = {}) => problems.push({ file, message, ...extra });
  const texts = new Map();
  const read = (file) => {
    if (!texts.has(file)) {
      try { texts.set(file, fs.readFileSync(path.join(root, file), 'utf8')); }
      catch (error) { texts.set(file, null); problem(file, `${file} cannot be read (${error.code ?? error.message}).`, { missing: true }); }
    }
    return texts.get(file);
  };
  const declared = ({ file, name, pattern }) => {
    const text = read(file);
    const value = text?.match(pattern)?.[1];
    if (text !== null && value === undefined) problem(file, `${file} no longer declares ${name} where scripts/check-versions.mjs looks for it; follow it there.`, { missing: true });
    return value;
  };

  // The agent.
  let agent = null;
  if (read('agent/package.json') !== null) {
    try { agent = JSON.parse(read('agent/package.json')); }
    catch (error) { problem('agent/package.json', `agent/package.json is not JSON: ${error.message}`); }
  }
  const version = agent?.version;
  if (agent && !STABLE.test(version ?? '')) problem('agent/package.json', `agent/package.json says version ${JSON.stringify(version)}, which is not an x.y.z version.`);
  const floorProblem = agent && agentFloorProblem(agent);
  if (floorProblem) problem('agent/package.json', floorProblem);

  if (STABLE.test(version ?? '')) {
    // What the agent reports in every reply, asked of config.mjs, the module the rest of the agent
    // imports it from, rather than read out of its text, so it holds however config.mjs comes by
    // it. It has to come from agent/package.json, which is signed and installed beside it: a
    // second copy is the one that gets forgotten. That copy is looked for in every module of the
    // agent, not in config.mjs alone: config.mjs passing on a literal written in another module
    // agrees today just the same.
    for (const file of agentModules(root)) {
      if (/\bAGENT_VERSION\s*=\s*['"`]/.test(read(file) ?? '')) problem(file, `${file} writes AGENT_VERSION out as a literal; read it from agent/package.json.`);
    }
    const config = 'agent/src/config.mjs';
    if (read(config) !== null) {
      try {
        const reported = (await import(pathToFileURL(path.join(root, config)).href)).AGENT_VERSION;
        if (reported !== version) problem(config, `${config} reports agent ${JSON.stringify(reported)}, but agent/package.json says ${version}.`);
      } catch (error) { problem(config, `${config} cannot be loaded: ${error.message}`); }
    }

    for (const copy of AGENT_COPIES) {
      const value = declared(copy);
      if (value !== undefined && value !== version) problem(copy.file, `${copy.file} says ${copy.name} = "${value}", but agent/package.json says ${version}.`);
    }

    const gradle = read(GRADLE);
    const fallback = gradle?.match(GRADLE_DEFAULT)?.[1];
    if (fallback !== undefined && fallback !== version) problem(GRADLE, `${GRADLE} bundles agent ${fallback} unless told otherwise, but agent/package.json says ${version}.`);
    if (gradle !== null && fallback === undefined && !GRADLE_READS_PACKAGE.test(gradleCode(gradle))) problem(GRADLE, `${GRADLE} names no default legion.agentVersion and does not read agent/package.json, so there is no telling which agent it bundles.`, { missing: true });
  }

  for (const file of BUNDLE_LOOKUPS) {
    const named = read(file)?.match(/legionctl-agent-\d+\.\d+\.\d+/)?.[0];
    if (named) problem(file, `${file} looks for ${named}.tgz by name; take the version from agent/package.json instead.`);
  }

  // contract/tools/make-fixtures.mjs writes the agent version and floor into every fixture of a
  // current agent's reply. It reads both from agent/package.json, so an agent release changes the
  // fixtures and generate.mjs --check fails until they are regenerated. A version written into the
  // generator would leave them describing the previous agent with nothing to notice.
  const generator = 'contract/tools/make-fixtures.mjs';
  const generatorText = read(generator);
  if (generatorText !== null) {
    if (!/readFileSync\([^;]*?(?:(['"])agent\1\s*,\s*(['"])package\.json\2|agent\/package\.json)/.test(generatorText)) problem(generator, `${generator} does not read agent/package.json.`);
    if (/\bconst AGENT\s*=\s*['"`]/.test(generatorText)) problem(generator, `${generator} writes the agent version out as a literal; take it from agent/package.json.`);
    if (/\bconst AGENT_FLOOR\s*=\s*['"`]/.test(generatorText) || !/\.agentFloor\b/.test(generatorText)) problem(generator, `${generator} does not take the version floor from legionControl.agentFloor in agent/package.json.`);
  }

  // The app.
  const appVersions = APP_VERSIONS.map(copy => ({ ...copy, value: declared(copy) })).filter(copy => copy.value !== undefined);
  const appBuilds = APP_BUILDS.map(copy => ({ ...copy, value: declared(copy) })).filter(copy => copy.value !== undefined);
  const [appVersion, appBuild] = [appVersions[0], appBuilds[0]];
  if (appVersion && !STABLE.test(appVersion.value)) problem(appVersion.file, `${appVersion.file} says ${appVersion.name} ${JSON.stringify(appVersion.value)}, which is not an x.y.z version.`);
  if (appBuild && !/^\d+$/.test(appBuild.value)) problem(appBuild.file, `${appBuild.file} says ${appBuild.name} ${JSON.stringify(appBuild.value)}, which is not a whole number.`);
  for (const [reference, copies] of [[appVersion, appVersions], [appBuild, appBuilds]]) {
    for (const copy of copies.slice(1)) {
      if (copy.value !== reference.value) problem(copy.file, `${copy.file} says ${copy.name} ${copy.value}, but ${reference.file} says ${reference.value}. release.sh bumps them together, so put them back in step.`);
    }
  }

  return { agent: { version, floor: agent?.legionControl?.agentFloor }, app: { version: appVersion?.value, build: appBuild?.value }, problems };
}

// Compared through realpath, as contract/validate.mjs does. Node resolves the symlinks in
// import.meta.url but not in argv[1], so through a symlinked checkout a plain comparison never
// runs the check and the gate passes having looked at nothing.
const real = (file) => { try { return fs.realpathSync(file); } catch { return path.resolve(file); } };
if (process.argv[1] && real(process.argv[1]) === real(fileURLToPath(import.meta.url))) {
  const result = await checkVersions(path.resolve(process.argv[2] ?? repository));
  for (const { message } of result.problems) console.error(message);
  if (result.problems.length) process.exitCode = 1;
  else console.log(`Versions agree: agent ${result.agent.version} (floor ${result.agent.floor}), app ${result.app.version} (build ${result.app.build}).`);
}
