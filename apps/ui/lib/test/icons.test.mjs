import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
import { entryIconName, FILE_CATEGORIES } from '../files.js';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const UI_ROOT = path.resolve(HERE, '..', '..');
const ICONS_PATH = path.join(UI_ROOT, 'icons.js');

function walkJs(dir) {
  const out = [];
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    if (entry.name === 'test' || entry.name === 'vendor' || entry.name === 'node_modules') continue;
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) out.push(...walkJs(full));
    else if (entry.name.endsWith('.js')) out.push(full);
  }
  return out;
}

function collectDefinedIcons() {
  const src = fs.readFileSync(ICONS_PATH, 'utf8');
  return new Set([...src.matchAll(/<symbol\s+id="i-([^"]+)"/g)].map((m) => m[1]));
}

function collectUsedIcons() {
  const used = new Map();
  const record = (name, loc) => {
    if (!used.has(name)) used.set(name, []);
    used.get(name).push(loc);
  };

  for (const file of walkJs(UI_ROOT)) {
    if (file === ICONS_PATH) continue;
    const rel = path.relative(UI_ROOT, file);
    const src = fs.readFileSync(file, 'utf8');

    // Static string literal: <${Icon} name="foo" />
    for (const m of src.matchAll(/<\$\{Icon\}\s+name="([^"]+)"/g)) {
      record(m[1], rel);
    }

    // Dynamic expression: <${Icon} name=${...} />
    for (const m of src.matchAll(/<\$\{Icon\}\s+name=\$\{([^}]+)\}/g)) {
      const expr = m[1];
      if (expr.includes('entryIconName')) {
        for (const cat of [...FILE_CATEGORIES, 'folder', 'dir', 'unknown']) {
          record(entryIconName(cat), `${rel} (entryIconName)`);
        }
      }
      // Extract any string literals in ternaries (e.g. `cond ? 'exit-fullscreen' : 'fullscreen'`).
      for (const lit of expr.matchAll(/['"]([a-z0-9-]+)['"]/g)) {
        // Skip comparison operands such as `=== 'button'` or `=== 'dir'` that sit before `?` or `===`.
        const before = expr.slice(0, lit.index);
        if (/(?:===|!==|==|!=)\s*$/.test(before)) continue;
        record(lit[1], `${rel} (ternary)`);
      }
    }
  }

  return used;
}

test('Every icon name used across views exists in icons.js and unused icons are accounted for', () => {
  const defined = collectDefinedIcons();
  const used = collectUsedIcons();

  assert.ok(defined.size > 0, 'Expected symbols in apps/ui/icons.js');

  const missing = [];
  for (const [name, locs] of used.entries()) {
    if (!defined.has(name)) {
      missing.push(`${name} (referenced in ${locs.join(', ')})`);
    }
  }
  assert.deepEqual(missing, [], `Icons referenced by views but missing from icons.js: ${missing.join('; ')}`);

  // Reverse check: explicitly track icons defined in icons.js that no view currently references.
  const unused = [...defined].filter((name) => !used.has(name)).sort();
  assert.deepEqual(
    unused,
    ['list', 'refresh', 'upload'],
    `Unused icon list changed (${unused.join(', ') || 'none'}); update this inventory if intentional`,
  );
});
