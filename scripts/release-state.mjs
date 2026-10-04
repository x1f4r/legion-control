#!/usr/bin/env node
// What GitHub says about a commit and a release, read for scripts/release.sh. Each question takes
// on standard input the JSON gh printed:
//
//   gh run list --commit SHA --json ...         | release-state.mjs ci SHA
//   gh release list --json tagName,isDraft,...  | release-state.mjs state X.Y.Z
//   gh release list --json tagName,isDraft,...  | release-state.mjs newer X.Y.Z "$(git ls-remote --tags origin)"
//   gh release view vX.Y.Z --json assets        | release-state.mjs assets FILE...
//
// Each prints one line. `state` prints none, draft or published. The others exit 1 when the release
// has to stop, and the line then says why; otherwise `newer` prints the newest version already out,
// or nothing when there is none.
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { compareVersions } from './check-versions.mjs';

const STABLE = /^\d+\.\d+\.\d+$/;

/**
 * Whether CI passed on `sha`. A workflow that ran on the commit more than once, a re-run or a run
 * for another event, counts by its newest run; every workflow that ran has to have passed, and one
 * at least has to have run.
 */
export function ciVerdict(runs, sha) {
  const commit = sha.slice(0, 12);
  const newest = new Map();
  for (const run of Array.isArray(runs) ? runs : []) {
    if (run?.headSha !== sha) continue;
    const workflow = run.workflowName || run.name || 'an unnamed workflow';
    if (!newest.has(workflow) || String(run.createdAt) > String(newest.get(workflow).createdAt)) newest.set(workflow, run);
  }
  if (newest.size === 0) return { ok: false, message: `CI has not run on ${commit}. A release is cut from a commit the checks passed on, so push it to main and wait for them.` };
  const running = [...newest].filter(([, run]) => run.status !== 'completed').map(([workflow]) => workflow);
  if (running.length) return { ok: false, message: `CI is still running on ${commit} (${running.join(', ')}). Wait for it to pass.` };
  const failed = [...newest].filter(([, run]) => run.conclusion !== 'success').map(([workflow, run]) => `${workflow}: ${run.conclusion || 'no conclusion'}`);
  if (failed.length) return { ok: false, message: `CI did not pass on ${commit} (${failed.join(', ')}). A release is cut from a commit every check passed on.` };
  return { ok: true, message: `CI passed on ${commit} (${[...newest.keys()].join(', ')}).` };
}

/** none, draft or published: what GitHub has for the tag vX.Y.Z. */
export function releaseState(releases, version) {
  const release = (Array.isArray(releases) ? releases : []).find(entry => entry?.tagName === `v${version}`);
  return !release ? 'none' : release.isDraft ? 'draft' : 'published';
}

/**
 * Whether `version` is above every version already out: every published release, and every vX.Y.Z
 * tag on origin, which claims its version even before a release is made of it. `tags` is what
 * `git ls-remote --tags origin` printed.
 */
export function newerVerdict(version, releases, tags = '') {
  const out = [
    ...(Array.isArray(releases) ? releases : []).filter(entry => entry && !entry.isDraft && !entry.isPrerelease).map(entry => ({ version: String(entry.tagName).replace(/^v/, ''), how: 'released' })),
    ...String(tags).split('\n').map(line => line.match(/\trefs\/tags\/v(\d+\.\d+\.\d+)(?:\^\{\})?$/)?.[1]).filter(Boolean).map(tagged => ({ version: tagged, how: 'tagged on origin' })),
  ].filter(entry => STABLE.test(entry.version));
  // Highest first; of two entries for one version the release is the one named.
  out.sort((a, b) => compareVersions(b.version, a.version) || (a.how === 'released' ? -1 : 1));
  const newest = out[0];
  if (newest && compareVersions(version, newest.version) <= 0) {
    return { ok: false, message: `${version} is not above ${newest.version}, the newest version already ${newest.how}. Versions only go up: a client on ${newest.version} is never offered ${version}.` };
  }
  return { ok: true, newest: newest?.version ?? '' };
}

/** Whether the release `view` holds every one of `files` ({ name, size }) at its exact size. */
export function assetVerdict(view, files) {
  const uploaded = new Map((Array.isArray(view?.assets) ? view.assets : []).map(asset => [asset?.name, asset]));
  const missing = files.filter(({ name, size }) => {
    const asset = uploaded.get(name);
    return !asset || asset.size !== size || (asset.state ?? 'uploaded') !== 'uploaded';
  }).map(({ name }) => name);
  if (missing.length) return { ok: false, message: `the draft release does not hold ${missing.join(', ')} as built.` };
  return { ok: true, message: `The draft release holds all ${files.length} assets.` };
}

// Compared through realpath, as the other release scripts do: Node resolves the symlinks in
// import.meta.url but not in argv[1], so through a symlinked checkout this would answer nothing.
const real = (file) => { try { return fs.realpathSync(file); } catch { return path.resolve(file); } };
if (process.argv[1] && real(process.argv[1]) === real(fileURLToPath(import.meta.url))) {
  const [question, ...args] = process.argv.slice(2);
  let input;
  try { input = JSON.parse(fs.readFileSync(0, 'utf8')); }
  catch (error) { console.log(`gh printed something that is not JSON (${error.message}).`); process.exit(1); }
  let verdict;
  if (question === 'ci' && args.length === 1) verdict = ciVerdict(input, args[0]);
  else if (question === 'state' && args.length === 1) verdict = { ok: true, message: releaseState(input, args[0]) };
  else if (question === 'newer' && args.length >= 1) {
    verdict = newerVerdict(args[0], input, args[1]);
    if (verdict.ok) verdict.message = verdict.newest;
  } else if (question === 'assets' && args.length >= 1) {
    verdict = assetVerdict(input, args.map(file => ({ name: path.basename(file), size: fs.statSync(file).size })));
  } else verdict = { ok: false, message: 'Usage: release-state.mjs ci SHA | state X.Y.Z | newer X.Y.Z [TAGS] | assets FILE...' };
  console.log(verdict.message);
  process.exitCode = verdict.ok ? 0 : 1;
}
