#!/usr/bin/env node
// Synchronizes or checks the 4 downstream media-type mirrors against the single source of truth
// in `crates/legion-core/src/media.rs` (`MEDIA_TABLE`).
//
// Usage:
//   node scripts/sync-media-table.mjs [root]          # regenerate mirrors in place
//   node scripts/sync-media-table.mjs --check [root]  # exit non-zero if any mirror differs

import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = path.resolve(__dirname, '..');

export const MIRROR_REL_PATHS = Object.freeze({
  core: 'crates/legion-core/src/media.rs',
  content: 'crates/legion-content/src/media.rs',
  policy: 'apps/legion-app/src-tauri/src/transfers/file_policy.rs',
  kotlin: 'apps/legion-app/src-tauri/gen/android/app/src/main/java/com/x1f4r/legion/TransferPathPolicy.kt',
  uiFiles: 'apps/ui/lib/files.js',
});

function normalizeNewlines(text) {
  return text.replace(/\r\n/g, '\n');
}

function detectEol(text) {
  return text.includes('\r\n') ? '\r\n' : '\n';
}

function applyEol(lfText, eol) {
  return eol === '\r\n' ? lfText.replace(/\n/g, '\r\n') : lfText;
}

function extractBetween(text, beginMarker, endMarker, label) {
  const start = text.indexOf(beginMarker);
  if (start < 0) throw new Error(`Missing marker ${beginMarker} in ${label}`);
  const afterStart = text.indexOf('\n', start);
  const end = text.indexOf(endMarker, afterStart + 1);
  if (end < 0) throw new Error(`Missing marker ${endMarker} in ${label}`);
  return text.slice(afterStart + 1, end);
}

function replaceBetween(text, beginMarker, endMarker, replacement, label) {
  const start = text.indexOf(beginMarker);
  if (start < 0) throw new Error(`Missing marker ${beginMarker} in ${label}`);
  const afterStart = text.indexOf('\n', start);
  const endLineStart = text.lastIndexOf('\n', text.indexOf(endMarker, afterStart + 1));
  if (endLineStart < afterStart) throw new Error(`Missing marker ${endMarker} in ${label}`);
  return `${text.slice(0, afterStart + 1)}${replacement}\n${text.slice(endLineStart + 1)}`;
}

function parseCoreTable(src, label) {
  const block = extractBetween(normalizeNewlines(src), '// BEGIN_MEDIA_TABLE', '// END_MEDIA_TABLE', label);
  const entries = [];
  const re =
    /entry\(\s*"([^"]+)",\s*Category::([A-Za-z]+),\s*"([^"]+)",\s*ContentServe::([A-Za-z]+)(?:\("([^"]+)"\))?,\s*(None|Some\("([^"]+)"\)),\s*(None|Some\("([^"]+)"\)),\s*ViewerMode::([A-Za-z]+)\s*,?\s*\)/g;
  const catMap = {
    Image: 'image',
    Video: 'video',
    Audio: 'audio',
    Document: 'document',
    Text: 'text',
    Archive: 'archive',
    Other: 'other',
  };
  const modeMap = {
    Inline: 'inline',
    ThumbOrFallback: 'thumb_or_fallback',
    Text: 'text',
    Fallback: 'fallback',
  };

  for (const m of block.matchAll(re)) {
    const ext = m[1];
    const category = catMap[m[2]];
    const mime = m[3];
    const serveKind = m[4];
    const serveArg = m[5] || null;
    const openMime = m[7] || null;
    const downloadMime = m[9] || null;
    const viewerMode = modeMap[m[10]];
    if (!category || !viewerMode) {
      throw new Error(`Invalid category/viewerMode in MEDIA_TABLE for ${ext}`);
    }
    let contentType = null;
    if (serveKind === 'InlineMedia') contentType = serveArg;
    else if (serveKind === 'InlineText') contentType = 'text/plain; charset=utf-8';
    else if (serveKind === 'InlineVtt') contentType = 'text/vtt; charset=utf-8';
    else if (serveKind === 'Attachment') contentType = null;
    else throw new Error(`Unknown ContentServe::${serveKind} for ${ext}`);
    entries.push({ ext, category, mime, contentType, openMime, downloadMime, viewerMode });
  }
  if (entries.length < 150) {
    throw new Error(`Expected >= 150 entries in MEDIA_TABLE, parsed only ${entries.length}`);
  }
  return entries;
}

function formatContentTable(entries) {
  const lines = ['const INLINE_CONTENT_TYPES: &[(&str, &str)] = &['];
  for (const e of entries) {
    if (e.contentType) {
      lines.push(`    ("${e.ext}", "${e.contentType}"),`);
    }
  }
  lines.push('];');
  return lines.join('\n');
}

function formatRustPairsConst(name, entries, pick) {
  const lines = [`pub const ${name}: &[(&str, &str)] = &[`];
  for (const e of entries) {
    const val = pick(e);
    if (val) {
      if (val.length > 56) {
        lines.push(`    (`);
        lines.push(`        "${e.ext}",`);
        lines.push(`        "${val}",`);
        lines.push(`    ),`);
      } else {
        lines.push(`    ("${e.ext}", "${val}"),`);
      }
    }
  }
  lines.push('];');
  return lines.join('\n');
}

function formatKotlinMap(name, entries, pick) {
  const lines = [`  internal val ${name}: Map<String, String> = mapOf(`];
  for (const e of entries) {
    const val = pick(e);
    if (val) {
      lines.push(`    "${e.ext}" to "${val}",`);
    }
  }
  lines.push('  )');
  return lines.join('\n');
}

function formatJsTable(entries) {
  const lines = ['const MEDIA_TABLE = {'];
  for (const e of entries) {
    const key = /^[a-z][a-z0-9]*$/.test(e.ext) ? e.ext : `'${e.ext}'`;
    const openFlag = e.openMime ? 'true' : 'false';
    lines.push(`  ${key}: ['${e.category}', '${e.mime}', '${e.viewerMode}', ${openFlag}],`);
  }
  lines.push('};');
  return lines.join('\n');
}

export function syncMediaTables({ repoRoot = REPO_ROOT, checkOnly = false } = {}) {
  const corePath = path.join(repoRoot, MIRROR_REL_PATHS.core);
  const contentPath = path.join(repoRoot, MIRROR_REL_PATHS.content);
  const policyPath = path.join(repoRoot, MIRROR_REL_PATHS.policy);
  const kotlinPath = path.join(repoRoot, MIRROR_REL_PATHS.kotlin);
  const uiFilesPath = path.join(repoRoot, MIRROR_REL_PATHS.uiFiles);

  const coreSrc = normalizeNewlines(fs.readFileSync(corePath, 'utf8'));
  const entries = parseCoreTable(coreSrc, corePath);

  const targets = [
    {
      path: contentPath,
      transform: (src) =>
        replaceBetween(
          src,
          '// BEGIN_CONTENT_MEDIA_TABLE',
          '// END_CONTENT_MEDIA_TABLE',
          formatContentTable(entries),
          contentPath,
        ),
    },
    {
      path: policyPath,
      transform: (src) => {
        let next = replaceBetween(
          src,
          '// BEGIN_ALLOWED_OPEN_MIME_BY_EXT',
          '// END_ALLOWED_OPEN_MIME_BY_EXT',
          formatRustPairsConst('ALLOWED_OPEN_MIME_BY_EXT', entries, (e) => e.openMime),
          policyPath,
        );
        next = replaceBetween(
          next,
          '// BEGIN_NON_OPEN_DOWNLOAD_MIME_BY_EXT',
          '// END_NON_OPEN_DOWNLOAD_MIME_BY_EXT',
          formatRustPairsConst('NON_OPEN_DOWNLOAD_MIME_BY_EXT', entries, (e) => e.downloadMime),
          policyPath,
        );
        return next;
      },
    },
    {
      path: kotlinPath,
      transform: (src) => {
        let next = replaceBetween(
          src,
          '// BEGIN_ALLOWED_OPEN_MIME_BY_EXT',
          '// END_ALLOWED_OPEN_MIME_BY_EXT',
          formatKotlinMap('ALLOWED_OPEN_MIME_BY_EXT', entries, (e) => e.openMime),
          kotlinPath,
        );
        next = replaceBetween(
          next,
          '// BEGIN_NON_OPEN_DOWNLOAD_MIME_BY_EXT',
          '// END_NON_OPEN_DOWNLOAD_MIME_BY_EXT',
          formatKotlinMap('NON_OPEN_DOWNLOAD_MIME_BY_EXT', entries, (e) => e.downloadMime),
          kotlinPath,
        );
        return next;
      },
    },
    {
      path: uiFilesPath,
      transform: (src) =>
        replaceBetween(
          src,
          '// BEGIN_MEDIA_TABLE',
          '// END_MEDIA_TABLE',
          formatJsTable(entries),
          uiFilesPath,
        ),
    },
  ];

  const drifted = [];
  const updated = [];
  for (const target of targets) {
    const rawBefore = fs.readFileSync(target.path, 'utf8');
    const eol = detectEol(rawBefore);
    const beforeLf = normalizeNewlines(rawBefore);
    const afterLf = target.transform(beforeLf);
    if (beforeLf !== afterLf) {
      const rel = path.relative(repoRoot, target.path);
      if (checkOnly) {
        drifted.push(rel);
      } else {
        fs.writeFileSync(target.path, applyEol(afterLf, eol));
        updated.push(rel);
      }
    }
  }

  return { entriesCount: entries.length, drifted, updated };
}

const real = (file) => {
  try {
    return fs.realpathSync(file);
  } catch {
    return path.resolve(file);
  }
};

if (process.argv[1] && real(process.argv[1]) === real(fileURLToPath(import.meta.url))) {
  const args = process.argv.slice(2);
  const checkOnly = args.includes('--check');
  const positional = args.filter((arg) => arg !== '--check');
  const repoRoot = positional[0] ? path.resolve(positional[0]) : REPO_ROOT;

  const { entriesCount, drifted, updated } = syncMediaTables({ repoRoot, checkOnly });
  if (checkOnly) {
    if (drifted.length > 0) {
      console.error(
        `Media table mirror(s) drifted from crates/legion-core/src/media.rs:\n  - ${drifted.join('\n  - ')}\nRun: node scripts/sync-media-table.mjs`,
      );
      process.exit(1);
    }
    console.log(`All 4 media table mirrors match crates/legion-core/src/media.rs (${entriesCount} types).`);
  } else {
    for (const rel of updated) {
      console.log(`Updated ${rel} (${entriesCount} canonical types)`);
    }
  }
}
