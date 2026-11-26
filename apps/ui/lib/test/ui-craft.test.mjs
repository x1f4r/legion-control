import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const UI_ROOT = path.resolve(HERE, '..', '..');

const NEVER_DISABLED_BUTTON_CLASSES = [
  'chip-btn',
  'update-note-dismiss',
  'player-center-btn',
  'player-speed-btn',
  'player-resume-btn',
  'music-ctl-btn',
  'music-mode-btn',
  'music-play-btn',
  'mini-player-btn',
];

// Buttons rendered inside these containers (including .swipe-action inside .swipe-actions).
const NEVER_DISABLED_CONTAINER_CLASSES = ['seg', 'tabs', 'filters', 'swipe-actions'];

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

// Read the opening tag body from `<tag` to its matching `>`, skipping `${...}` template expressions
// and quoted attribute values so `=>` or `>` inside JS expressions never splits a tag.
function readOpeningTagEnd(src, start) {
  let i = start;
  let quote = null;
  let exprDepth = 0;
  while (i < src.length) {
    const ch = src[i];
    if (quote) {
      if (ch === '\\') {
        i += 2;
        continue;
      }
      if (ch === quote) quote = null;
      i += 1;
      continue;
    }
    if (exprDepth > 0) {
      if (ch === '"' || ch === "'" || ch === '`') {
        quote = ch;
      } else if (ch === '{') {
        exprDepth += 1;
      } else if (ch === '}') {
        exprDepth -= 1;
      }
      i += 1;
      continue;
    }
    if (ch === '$' && src[i + 1] === '{') {
      exprDepth = 1;
      i += 2;
      continue;
    }
    if (ch === '"' || ch === "'") {
      quote = ch;
      i += 1;
      continue;
    }
    if (ch === '>') return i;
    i += 1;
  }
  return -1;
}

function extractClassTokens(tagText) {
  const tokens = new Set();
  for (const m of tagText.matchAll(/\bclass="([^"]*)"/g)) {
    for (const tok of m[1].trim().split(/\s+/)) {
      if (tok) tokens.add(tok);
    }
  }
  for (const m of tagText.matchAll(/\bclass=\$\{([^}]+)\}/g)) {
    for (const lit of m[1].matchAll(/['"`]([^'"`]+)['"`]/g)) {
      for (const tok of lit[1].trim().split(/\s+/)) {
        if (tok) tokens.add(tok);
      }
    }
  }
  return tokens;
}

function hasDisabledAttribute(tagText) {
  // Strip quoted strings and template expressions' inner bodies so a variable named `disabled`
  // inside class/onClick isn't misread; only match an actual `disabled` HTML attribute.
  let out = '';
  let i = 0;
  let quote = null;
  let exprDepth = 0;
  while (i < tagText.length) {
    const ch = tagText[i];
    if (quote) {
      if (ch === '\\') {
        i += 2;
        continue;
      }
      if (ch === quote) quote = null;
      i += 1;
      continue;
    }
    if (exprDepth > 0) {
      if (ch === '"' || ch === "'" || ch === '`') quote = ch;
      else if (ch === '{') exprDepth += 1;
      else if (ch === '}') exprDepth -= 1;
      i += 1;
      continue;
    }
    if (ch === '$' && tagText[i + 1] === '{') {
      exprDepth = 1;
      out += ' ';
      i += 2;
      continue;
    }
    if (ch === '"' || ch === "'") {
      quote = ch;
      out += ' ';
      i += 1;
      continue;
    }
    out += ch;
    i += 1;
  }
  return /\bdisabled(?=\s|=|\/|>|$)/.test(out);
}

// Find all `<button ...>` elements inside matching `<div class="<container>">...</div>` blocks.
function findButtonsInContainers(src) {
  const hits = [];
  const tagRe = /<(\/?)(div|button)\b/g;
  const containerStack = [];
  let match;
  while ((match = tagRe.exec(src)) !== null) {
    const isClose = match[1] === '/';
    const tagName = match[2];
    const end = readOpeningTagEnd(src, match.index + match[0].length);
    if (end === -1) break;
    tagRe.lastIndex = end + 1;
    const tagText = src.slice(match.index, end + 1);
    const selfClosing = /\/\s*>$/.test(tagText);

    if (tagName === 'div') {
      if (isClose) {
        containerStack.pop();
      } else if (!selfClosing) {
        const classes = extractClassTokens(tagText);
        const matchedContainer = NEVER_DISABLED_CONTAINER_CLASSES.find((c) => classes.has(c)) || null;
        containerStack.push(matchedContainer);
      }
    } else if (tagName === 'button' && !isClose) {
      const classes = extractClassTokens(tagText);
      const activeContainer = [...containerStack].reverse().find(Boolean) || null;
      const line = src.slice(0, match.index).split('\n').length;
      hits.push({ line, tagText, classes, container: activeContainer });
    }
  }
  return hits;
}

test('Never-disabled controls are never given a disabled attribute in any view', () => {
  const seenButtonClasses = new Set();
  const seenContainers = new Set();
  const violations = [];

  for (const file of walkJs(UI_ROOT)) {
    const rel = path.relative(UI_ROOT, file);
    const src = fs.readFileSync(file, 'utf8');
    for (const btn of findButtonsInContainers(src)) {
      const matchedClasses = NEVER_DISABLED_BUTTON_CLASSES.filter((c) => btn.classes.has(c));
      for (const c of matchedClasses) seenButtonClasses.add(c);
      if (btn.container) seenContainers.add(btn.container);

      if (
        (matchedClasses.length > 0 || btn.classes.has('swipe-action') || btn.container) &&
        hasDisabledAttribute(btn.tagText)
      ) {
        const label =
          matchedClasses.length > 0
            ? `.${matchedClasses.join('.')}`
            : btn.classes.has('swipe-action')
              ? '.swipe-action'
              : `.${btn.container} button`;
        violations.push(`${rel}:${btn.line} (${label})`);
      }
    }
  }

  // Guard against silent class renames making the check vacuous.
  assert.deepEqual(
    [...seenButtonClasses].sort(),
    [...NEVER_DISABLED_BUTTON_CLASSES].sort(),
    'Expected every tracked never-disabled button class to appear in apps/ui/',
  );
  assert.deepEqual(
    [...seenContainers].sort(),
    [...NEVER_DISABLED_CONTAINER_CLASSES].sort(),
    'Expected every tracked never-disabled container class to appear in apps/ui/',
  );
  assert.deepEqual(
    violations,
    [],
    `Controls audited as never-disabled must not carry a disabled attribute (if a disabled state is added, style it with the shared unavailable look and update the audit): ${violations.join(', ')}`,
  );
});
