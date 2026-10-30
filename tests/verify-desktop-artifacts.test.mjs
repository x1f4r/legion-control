import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import test from 'node:test';

const repository = path.resolve(import.meta.dirname, '..');
const script = path.join(repository, 'scripts', 'verify-desktop-artifacts.mjs');
const PLATFORMS = ['linux-x64', 'linux-arm64', 'windows-x64'];

// The check reads the archives with tar and unzip, and the test writes them with tar and zip. A
// release is cut on a Mac, and CI checks real archives on Linux; on Windows the tar a shell finds
// first takes C: in a path for a remote host.
const skip = process.platform === 'win32' ? 'releases are not cut on Windows'
  : !['tar', 'zip', 'unzip'].every(tool => !spawnSync(tool, ['--help'], { stdio: 'ignore' }).error) && 'needs tar, zip and unzip';

// The three archives laid out the way desktop/build.sh writes them, each holding only the runtime
// metadata the check reads, which names the desktop project at `versions[platform]`.
function archives(t, versions) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'legion-desktop-artifacts-'));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const dist = path.join(root, 'dist');
  fs.mkdirSync(dist);
  for (const platform of PLATFORMS) {
    if (versions[platform] === null) continue;
    const staging = path.join(root, platform);
    fs.mkdirSync(staging);
    const libraries = { [`legion-control/${versions[platform]}`]: { type: 'project' }, 'Avalonia/11.3.20': { type: 'package' } };
    fs.writeFileSync(path.join(staging, 'legion-control.deps.json'), JSON.stringify({ libraries }));
    const made = platform === 'windows-x64'
      ? spawnSync('zip', ['-q', '-r', '-X', path.join(dist, 'Legion-Control-windows-x64.zip'), '.'], { cwd: staging })
      : spawnSync('tar', ['-czf', path.join(dist, `Legion-Control-${platform}.tar.gz`), '-C', staging, '.']);
    assert.equal(made.status, 0, String(made.stderr));
  }
  return (version) => spawnSync(process.execPath, [script, version, dist], { encoding: 'utf8' });
}
const all = (version) => Object.fromEntries(PLATFORMS.map(platform => [platform, version]));

test('desktop archives that all carry the release version pass', { skip }, (t) => {
  const result = archives(t, all('1.4.0'))('1.4.0');
  assert.equal(result.status, 0, result.stderr);
  assert.equal(result.stdout, PLATFORMS.map(platform => `${platform} archive contains desktop 1.4.0\n`).join(''));
});

test('an archive built from another version, or missing, stops the release', { skip }, (t) => {
  const stale = archives(t, { ...all('1.4.0'), 'linux-arm64': '1.3.4' })('1.4.0');
  assert.equal(stale.status, 1);
  assert.match(stale.stderr, /linux-arm64 archive identifies legion-control\/1\.3\.4, expected legion-control\/1\.4\.0/);

  const missing = archives(t, { ...all('1.4.0'), 'windows-x64': null })('1.4.0');
  assert.equal(missing.status, 1);
  assert.match(missing.stderr, /Cannot read windows-x64 runtime metadata/);
});

test('the check refuses a version that is not x.y.z', (t) => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'legion-desktop-artifacts-'));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const result = spawnSync(process.execPath, [script, 'v1.4.0', root], { encoding: 'utf8' });
  assert.equal(result.status, 1);
  assert.match(result.stderr, /Usage: verify-desktop-artifacts\.mjs X\.Y\.Z \[directory\]/);
});
