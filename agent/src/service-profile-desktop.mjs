// Read application metadata without starting the application or its updater.
import fs from 'node:fs';
import path from 'node:path';
import { spawnSync } from 'node:child_process';

export function readDesktopMetadata(location, { platform = process.platform, io = fs, run = spawnSync } = {}) {
  try {
    if (typeof location !== 'string' || !path.isAbsolute(location)) return null;
    let metadata;
    if (platform === 'darwin') {
      if (!location.endsWith('.app')) return null;
      const plist = path.join(location, 'Contents', 'Info.plist');
      const stat = io.statSync(plist);
      if (!stat.isFile() || stat.size > 1024 * 1024) return null;
      const result = run('/usr/bin/plutil', ['-convert', 'json', '-o', '-', plist], { encoding: 'utf8', timeout: 3000, maxBuffer: 1024 * 1024 });
      if (result.error || result.status !== 0) return null;
      const value = JSON.parse(result.stdout);
      metadata = { version: value.CFBundleShortVersionString ?? value.CFBundleVersion, identity: value.CFBundleIdentifier };
    } else if (platform === 'win32') {
      if (!location.toLowerCase().endsWith('.exe') || !io.statSync(location).isFile()) return null;
      const result = run('powershell.exe', ['-NoProfile', '-NonInteractive', '-Command', "$ErrorActionPreference='Stop'; $v=[System.Diagnostics.FileVersionInfo]::GetVersionInfo($env:LEGION_PROFILE_EXECUTABLE); @{version=$v.ProductVersion;identity=$v.ProductName} | ConvertTo-Json -Compress"], { env: { ...process.env, LEGION_PROFILE_EXECUTABLE: location }, encoding: 'utf8', timeout: 3000, maxBuffer: 65536, windowsHide: true });
      if (result.error || result.status !== 0) return null;
      metadata = JSON.parse(result.stdout);
    } else return null;
    if (typeof metadata.version !== 'string' || !/^\d+(?:\.\d+){1,5}(?:[-+][A-Za-z0-9.-]+)?$/.test(metadata.version)) return null;
    if (typeof metadata.identity !== 'string' || !metadata.identity || metadata.identity.length > 200 || /[\x00-\x1f]/.test(metadata.identity)) return null;
    return { version: metadata.version, identity: metadata.identity };
  } catch { return null; }
}
