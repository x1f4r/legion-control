#!/usr/bin/env node
// Check the published runtime metadata inside every desktop archive before signing.
import { spawnSync } from 'node:child_process';
import path from 'node:path';
const [version, directory = 'dist'] = process.argv.slice(2);
if (!/^\d+\.\d+\.\d+$/.test(version ?? '')) throw new Error('Usage: verify-desktop-artifacts.mjs X.Y.Z [directory]');
for (const platform of ['linux-x64', 'linux-arm64', 'windows-x64']) {
  const args = platform.startsWith('linux-')
    ? ['-xOzf', path.join(directory, `Legion-Control-${platform}.tar.gz`), './legion-control.deps.json']
    : ['-p', path.join(directory, 'Legion-Control-windows-x64.zip'), 'legion-control.deps.json'];
  const result = spawnSync(platform.startsWith('linux-') ? 'tar' : 'unzip', args, { encoding: 'utf8', maxBuffer: 8 * 1024 * 1024 });
  if (result.status !== 0) throw new Error(`Cannot read ${platform} runtime metadata: ${result.stderr || result.error}`);
  const metadata = JSON.parse(result.stdout);
  const project = Object.entries(metadata.libraries ?? {}).find(([, value]) => value.type === 'project');
  if (project?.[0] !== `legion-control/${version}`) throw new Error(`${platform} archive identifies ${project?.[0]}, expected legion-control/${version}`);
  console.log(`${platform} archive contains desktop ${version}`);
}
