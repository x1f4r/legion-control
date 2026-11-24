#!/usr/bin/env node
// Fail if a compiled binary embeds the build host's workspace, home, or Cargo directory paths.
//
// Usage:
//   node scripts/check-binary-paths.mjs <binary> [--forbid <path>]...
//
// By default, checks the current working directory, the user's home directory, GITHUB_WORKSPACE,
// HOME, USERPROFILE, CARGO_HOME, and RUSTUP_HOME (plus their resolved realpaths when they exist),
// along with any extra paths passed via --forbid.
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

function trimTrailingSeparators(value) {
  let out = value.trim();
  while (out.length > 1 && (out.endsWith('/') || out.endsWith('\\'))) {
    // Keep a bare Windows drive root such as C:\ intact so the root filter below drops it cleanly.
    if (/^[A-Za-z]:[\\/]$/.test(out)) break;
    out = out.slice(0, -1);
  }
  return out;
}

function isUsablePath(value) {
  if (!value || value.length < 4) return false;
  if (value === '/' || value === '\\') return false;
  if (/^[A-Za-z]:[\\/]?$/.test(value)) return false;
  return true;
}

function expandPathVariants(rawPath) {
  const cleaned = trimTrailingSeparators(rawPath);
  if (!isUsablePath(cleaned)) return [];

  const bases = new Set([cleaned]);
  try {
    const resolved = trimTrailingSeparators(fs.realpathSync(cleaned));
    if (isUsablePath(resolved)) bases.add(resolved);
  } catch {
    // Path may belong to another platform or not exist locally (for example in unit tests).
  }

  const variants = new Map();
  const addVariant = (text, encoding, caseInsensitive) => {
    const key = `${encoding}:${caseInsensitive ? 'i' : 's'}:${caseInsensitive ? text.toLowerCase() : text}`;
    if (!variants.has(key)) {
      variants.set(key, { text, encoding, caseInsensitive });
    }
  };

  for (const base of bases) {
    const slash = base.replace(/\\/g, '/');
    const isWindowsDrive = /^[A-Za-z]:\//.test(slash);
    if (isWindowsDrive) {
      const backslash = slash.replace(/\//g, '\\');
      addVariant(backslash, 'utf8', true);
      addVariant(slash, 'utf8', true);
      addVariant(backslash, 'utf16le', true);
      addVariant(slash, 'utf16le', true);
    } else {
      addVariant(base, 'utf8', false);
      if (base.includes('\\')) {
        addVariant(slash, 'utf8', false);
      }
    }
  }
  return [...variants.values()];
}

function asciiLowerBuffer(buf) {
  const out = Buffer.allocUnsafe(buf.length);
  for (let i = 0; i < buf.length; i += 1) {
    const b = buf[i];
    out[i] = b >= 0x41 && b <= 0x5a ? b + 0x20 : b;
  }
  return out;
}

/**
 * Collect the default host paths that must never appear inside a shipped release binary.
 */
export function collectDefaultPaths({
  env = process.env,
  cwd = process.cwd(),
  homedir = os.homedir(),
} = {}) {
  const entries = [
    ['cwd', cwd],
    ['homedir', homedir],
    ['GITHUB_WORKSPACE', env.GITHUB_WORKSPACE],
    ['HOME', env.HOME],
    ['USERPROFILE', env.USERPROFILE],
    ['CARGO_HOME', env.CARGO_HOME],
    ['RUSTUP_HOME', env.RUSTUP_HOME],
  ];
  const seen = new Set();
  const out = [];
  for (const [label, value] of entries) {
    if (!value) continue;
    const cleaned = trimTrailingSeparators(value);
    if (!isUsablePath(cleaned) || seen.has(cleaned)) continue;
    seen.add(cleaned);
    out.push({ label, path: cleaned });
  }
  return out;
}

/**
 * Inspect `binaryPath` for any occurrence of `forbiddenPaths` (strings or `{ label, path }` objects).
 */
export function checkBinaryPaths(binaryPath, forbiddenPaths) {
  const bytes = fs.readFileSync(binaryPath);
  let lowerBytes = null;
  const getLowerBytes = () => {
    if (!lowerBytes) lowerBytes = asciiLowerBuffer(bytes);
    return lowerBytes;
  };

  const checked = [];
  const matches = [];
  const seenMatch = new Set();

  for (const item of forbiddenPaths) {
    const label = typeof item === 'string' ? 'path' : (item.label ?? 'path');
    const raw = typeof item === 'string' ? item : item.path;
    const variants = expandPathVariants(raw);
    if (variants.length === 0) continue;
    checked.push({ label, path: trimTrailingSeparators(raw) });

    for (const variant of variants) {
      const needleText = variant.caseInsensitive ? variant.text.toLowerCase() : variant.text;
      const needle = Buffer.from(needleText, variant.encoding);
      const haystack = variant.caseInsensitive ? getLowerBytes() : bytes;
      if (haystack.indexOf(needle) !== -1) {
        const key = `${label}:${variant.encoding}:${variant.text}`;
        if (!seenMatch.has(key)) {
          seenMatch.add(key);
          matches.push({ label, path: variant.text, encoding: variant.encoding });
        }
      }
    }
  }

  return {
    ok: matches.length === 0,
    binary: binaryPath,
    checked,
    matches,
  };
}

function parseCliArgs(argv) {
  let binaryPath = null;
  const extra = [];
  let useDefaults = true;
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === '--forbid') {
      const next = argv[i + 1];
      if (!next) throw new Error('--forbid requires a path argument.');
      extra.push({ label: 'forbid', path: next });
      i += 1;
    } else if (arg === '--no-defaults') {
      useDefaults = false;
    } else if (arg === '--help' || arg === '-h') {
      console.log('Usage: node scripts/check-binary-paths.mjs <binary> [--forbid <path>]... [--no-defaults]');
      process.exit(0);
    } else if (arg.startsWith('-')) {
      throw new Error(`Unknown option: ${arg}`);
    } else if (!binaryPath) {
      binaryPath = arg;
    } else {
      throw new Error(`Unexpected extra argument: ${arg}`);
    }
  }
  if (!binaryPath) {
    throw new Error('Usage: node scripts/check-binary-paths.mjs <binary> [--forbid <path>]...');
  }
  return { binaryPath, extra, useDefaults };
}

const real = (file) => {
  try {
    return fs.realpathSync(file);
  } catch {
    return path.resolve(file);
  }
};

if (process.argv[1] && real(process.argv[1]) === real(fileURLToPath(import.meta.url))) {
  try {
    const { binaryPath, extra, useDefaults } = parseCliArgs(process.argv.slice(2));
    const forbidden = useDefaults ? [...collectDefaultPaths(), ...extra] : extra;
    if (forbidden.length === 0) {
      throw new Error('No paths to check (pass --forbid <path> or run without --no-defaults).');
    }
    const result = checkBinaryPaths(binaryPath, forbidden);
    if (!result.ok) {
      console.error(`Build host path leaked into ${result.binary}:`);
      for (const m of result.matches) {
        console.error(`  - [${m.label}] ${m.path} (${m.encoding})`);
      }
      process.exit(1);
    }
    console.log(`Checked ${result.binary}: no build host paths found (${result.checked.length} prefixes checked).`);
  } catch (error) {
    console.error(`check-binary-paths.mjs: ${error.message}`);
    process.exit(2);
  }
}
