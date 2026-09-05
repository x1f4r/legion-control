// The canonical form of a controller document, and its hash.
//
// This is the reference implementation of the rule in v3.md, section "Canonical
// controller bytes". It is the only place in this repository where the rule is
// written as code; the agent and the three clients each implement it again in
// their own language, and contract/hash-vectors.json is what proves the four
// implementations agree.
//
// The steps are ordered and the order matters. Every one of them exists because
// some editor, some shell redirect or some phone keyboard produces bytes that a
// person would call the same document and a hash would not.

import crypto from 'node:crypto';

/**
 * The exact set of code points stripped from the two ends of the document:
 * tab, line feed, vertical tab, form feed, carriage return, space.
 *
 * Spelled out rather than delegated to a language's own trim, because those do
 * not agree: JavaScript's String.trim also strips U+00A0 and U+FEFF, Java's
 * String.strip follows Character.isWhitespace, and .NET's Trim uses yet another
 * table. A document that begins with a non-breaking space must hash the same on
 * all four sides, so the set is fixed here and nowhere else.
 */
export const TRIM_CODE_POINTS = ['\u0009', '\u000a', '\u000b', '\u000c', '\u000d', '\u0020'];

const TRIM = new Set(TRIM_CODE_POINTS);

/** Whether these bytes are valid UTF-8. Invalid bytes are refused, never hashed. */
export function isValidUtf8(bytes) {
  const buffer = Buffer.isBuffer(bytes) ? bytes : Buffer.from(bytes);
  // A round trip through the decoder loses nothing when the input was valid, and
  // replaces every bad byte with U+FFFD when it was not.
  return Buffer.compare(Buffer.from(buffer.toString('utf8'), 'utf8'), buffer) === 0;
}

/**
 * The canonical bytes of a controller document.
 *
 * Throws on input that is not valid UTF-8: hashing replacement characters would
 * hand back a stable hash for a document nobody can read, and the two sides
 * would then agree forever about rubbish.
 */
export function canonicalBytes(bytes) {
  const buffer = Buffer.isBuffer(bytes) ? bytes : Buffer.from(bytes);
  if (!isValidUtf8(buffer)) throw new Error('the document is not valid UTF-8');

  let text = buffer.toString('utf8');

  // 1. One leading byte order mark, and only a leading one. A BOM further in is
  //    a zero width no-break space inside the document and stays where it is.
  if (text.charCodeAt(0) === 0xfeff) text = text.slice(1);

  // 2. Line endings. CRLF first, then any remaining lone CR, so a CRLF file does
  //    not turn into a blank line between every pair of lines.
  text = text.replace(/\r\n/g, '\n').replace(/\r/g, '\n');

  // 3. Both ends, using the fixed set above.
  let start = 0;
  let end = text.length;
  while (start < end && TRIM.has(text[start])) start += 1;
  while (end > start && TRIM.has(text[end - 1])) end -= 1;
  text = text.slice(start, end);

  // 4. Exactly one trailing newline, whether the editor left none or twelve.
  return Buffer.from(`${text}\n`, 'utf8');
}

/** Lowercase hex sha256 of the canonical bytes. */
export function canonicalHash(bytes) {
  return crypto.createHash('sha256').update(canonicalBytes(bytes)).digest('hex');
}
