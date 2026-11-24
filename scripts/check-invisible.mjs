#!/usr/bin/env node
// Check that no file in the checkout holds a zero-width, bidirectional or byte order mark
// character.
//
//   node scripts/check-invisible.mjs               this checkout
//   node scripts/check-invisible.mjs <directory>   another one
//
// Such a character makes text read differently from what it is: a bidirectional override can
// reorder a line on screen, and a zero-width space can split a word nobody sees split. This check
// reads every text file git tracks or would track. A test or a comment that means such a character
// writes it as an escape. One byte order mark at the start of a .ps1 is allowed, which is how
// Windows PowerShell 5.1 knows a script is UTF-8.
//
// scripts/check.sh runs it.
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { checkoutFiles } from './checkout-files.mjs';

const repository = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');

/** Zero-width spaces, joiners and marks, embeddings, overrides and isolates, word joiners and the byte order mark. */
export const INVISIBLE = /[\u200B-\u200F\u202A-\u202E\u2060-\u2069\uFEFF]/gu;

/**
 * The lines of `text` that hold an invisible character, each with the code
 * points it holds. A byte order mark that opens a .ps1 is allowed: it is how
 * Windows PowerShell 5.1 knows a script is UTF-8 rather than the ANSI code
 * page, and an installer may need one.
 */
export function invisibleCharacters(text, file) {
  const signed = path.extname(file).toLowerCase() === '.ps1' && text.startsWith('\uFEFF');
  return (signed ? text.slice(1) : text).split('\n').flatMap((line, index) => {
    const points = [...line.matchAll(INVISIBLE)].map(([character]) => `U+${character.codePointAt(0).toString(16).toUpperCase().padStart(4, '0')}`);
    return points.length > 0 ? [{ line: index + 1, points }] : [];
  });
}

/**
 * Every text file of the checkout under `root` and the lines of each that hold an invisible
 * character: `{ files, problems: [{ file, line, points }] }`. A file with a NUL in its first
 * kilobytes is binary, as git reads it, and is skipped.
 */
export function checkInvisible(root = repository) {
  const problems = [];
  let files = 0;
  for (const file of checkoutFiles(root)) {
    let bytes;
    try { bytes = fs.readFileSync(path.join(root, file)); } catch { continue; }
    if (bytes.subarray(0, 8000).includes(0)) continue;
    files += 1;
    for (const { line, points } of invisibleCharacters(bytes.toString('utf8'), file)) problems.push({ file, line, points });
  }
  return { files, problems };
}

const real = (file) => { try { return fs.realpathSync(file); } catch { return path.resolve(file); } };
if (process.argv[1] && real(process.argv[1]) === real(fileURLToPath(import.meta.url))) {
  let result;
  try {
    result = checkInvisible(path.resolve(process.argv[2] ?? repository));
  } catch (error) {
    console.error(error.message);
    process.exit(2);
  }
  for (const { file, line, points } of result.problems) {
    console.error(`${file}:${line}: ${points.join(', ')}; write it as an escape.`);
  }
  if (result.problems.length > 0) {
    console.error(`${result.problems.length} lines in ${new Set(result.problems.map(({ file }) => file)).size} of ${result.files} files hold invisible or bidirectional characters.`);
    process.exitCode = 1;
  } else {
    console.log(`No invisible or bidirectional characters in ${result.files} files.`);
  }
}
