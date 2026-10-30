// Zero-width, bidirectional and byte order marks make source text read
// differently from how it runs, which is how a reviewer approves a guard that
// does something else. The agent refuses them in anything a person approves,
// so its own code writes them as \u escapes, never as the characters, and so
// does this check: a class written with the characters it looks for is one
// nobody can read, and the forge flags it.

import fs from 'node:fs';
import path from 'node:path';

/** Zero-width spaces, joiners and marks, embeddings, overrides and isolates, word joiners and the byte order mark. */
export const INVISIBLE = /[\u200B-\u200F\u202A-\u202E\u2060-\u2069\uFEFF]/gu;

/** The kinds of text the agent's directories hold. A Finder .DS_Store or an editor's swap file is not one. */
const TEXT = /^[^.].*\.(mjs|cjs|js|json|sh|ps1|service|timer)$/;

/**
 * Every file under `agent` a person reviews: the sources and install files
 * that ship, package.json, and the scripts and tests that vouch for them,
 * this check among them.
 */
export function reviewedFiles(agent) {
  const files = ['src', 'install', 'scripts', 'test'].flatMap((directory) => (
    fs.readdirSync(path.join(agent, directory), { recursive: true })
      .filter((name) => TEXT.test(path.basename(name)))
      .map((name) => path.join(agent, directory, name))
      .filter((file) => fs.statSync(file).isFile())
  ));
  return [...files, path.join(agent, 'package.json')].sort();
}

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
