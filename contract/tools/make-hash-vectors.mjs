// Writes contract/hash-vectors.json.
//
// Every vector is a pair of byte strings and the hash they must produce. The
// inputs are the shapes a real controller document arrives in: a file the Mac
// wrote, the same file after a Windows editor touched it, the same document
// pasted into a phone keyboard that added a leading blank line, a document with
// a name in it that is not ASCII. The point of each one is written into its
// `purpose`, because a vector nobody understands gets deleted the first time it
// fails.
//
// Run: node contract/tools/make-hash-vectors.mjs

import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { canonicalBytes, canonicalHash, isValidUtf8 } from './canonical.mjs';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const OUT = path.join(HERE, '..', 'hash-vectors.json');

// One realistic document, in the compact form the Mac writes, used by most of
// the vectors so that the hash they share is visibly the same number.
const DOC = [
  '{',
  '  "version": 1,',
  '  "machines": [',
  '    {',
  '      "id": "atlas",',
  '      "name": "Atlas",',
  '      "endpoints": [{ "host": "100.113.65.2", "user": "x1f4r", "port": 22 }]',
  '    }',
  '  ]',
  '}',
].join('\n');

const NON_ASCII = [
  '{',
  '  "version": 1,',
  '  "machines": [',
  '    { "id": "buro", "name": "Büro — 大堂 ☀️", "endpoints": [] }',
  '  ]',
  '}',
].join('\n');

const BOM = '\ufeff';
const NBSP = '\u00a0';

/**
 * A vector.
 *
 * `input` is given as bytes so a case can carry a byte sequence that is not
 * valid text at all, which is exactly the case an implementation is most likely
 * to hash instead of refusing.
 */
function vector(name, purpose, inputBytes, { accepted = true, reject = null } = {}) {
  const buffer = Buffer.isBuffer(inputBytes) ? inputBytes : Buffer.from(inputBytes, 'utf8');
  const valid = isValidUtf8(buffer);
  const entry = {
    name,
    purpose,
    inputBase64: buffer.toString('base64'),
    inputBytes: buffer.length,
    canonicalBase64: valid ? canonicalBytes(buffer).toString('base64') : null,
    canonicalBytes: valid ? canonicalBytes(buffer).length : null,
    sha256: valid ? canonicalHash(buffer) : null,
    accepted,
    reject,
  };
  return entry;
}

const vectors = [
  vector(
    'plain-no-trailing-newline',
    'The baseline. Every other vector that ends in the same document must produce this same hash.',
    DOC,
  ),
  vector(
    'plain-one-trailing-newline',
    'A file the editor ended properly. Same document, so the same hash as plain-no-trailing-newline.',
    `${DOC}\n`,
  ),
  vector(
    'many-trailing-newlines',
    'Three blank lines at the end collapse to exactly one newline, not to three and not to none.',
    `${DOC}\n\n\n\n`,
  ),
  vector(
    'crlf',
    'The same document after a Windows editor. Every CRLF becomes one LF; the hash does not change.',
    DOC.replace(/\n/g, '\r\n'),
  ),
  vector(
    'crlf-and-trailing-crlf',
    'CRLF throughout and a CRLF at the end. Catches an implementation that converts endings after trimming.',
    `${DOC.replace(/\n/g, '\r\n')}\r\n`,
  ),
  vector(
    'cr-only',
    'Lone carriage returns, as a classic Mac editor or a mangled transfer leaves them. Each becomes one LF.',
    DOC.replace(/\n/g, '\r'),
  ),
  vector(
    'bom',
    'A byte order mark from a Windows text editor. Stripped, so the hash matches the plain document.',
    `${BOM}${DOC}`,
  ),
  vector(
    'bom-crlf-trailing-blank-lines',
    'The full Windows treatment at once: BOM, CRLF, and blank lines at the end. Still the same hash.',
    `${BOM}${DOC.replace(/\n/g, '\r\n')}\r\n\r\n`,
  ),
  vector(
    'leading-blank-lines-and-spaces',
    'A document pasted into a text field that added a blank line and some indentation in front of it.',
    `\n\n   \t${DOC}`,
  ),
  vector(
    'surrounded-by-whitespace',
    'Whitespace of every kind in the trim set on both ends: tab, vertical tab, form feed, space, newline.',
    ` \t\n${DOC}\n \t`,
  ),
  vector(
    'non-ascii',
    'Umlauts, an em dash, CJK and an emoji with a variation selector. Hashed as UTF-8 bytes, not as UTF-16 units.',
    NON_ASCII,
  ),
  vector(
    'non-ascii-bom-crlf',
    'The non-ASCII document after a round trip through a Windows editor. Same hash as non-ascii.',
    `${BOM}${NON_ASCII.replace(/\n/g, '\r\n')}\r\n`,
  ),
  vector(
    'escaped-crlf-inside-string',
    'A backslash-r backslash-n pair inside a JSON string is two characters of content and must survive untouched. An implementation that unescapes before normalising fails here.',
    '{"version":1,"machines":[],"note":"line one\\r\\nline two"}',
  ),
  vector(
    'literal-cr-inside-string-is-not-json',
    'A raw carriage return inside a JSON string. Normalisation turns it into a raw newline, and both are illegal in JSON, so the document is refused rather than repaired.',
    '{"version":1,"machines":[],"note":"line one\rline two"}',
    { accepted: false, reject: 'not-a-json-object' },
  ),
  vector(
    'bom-inside-document',
    'A byte order mark that is not the first code point is a zero width no-break space inside the document. It stays, so this hash differs from the plain one.',
    `{"version":1,"machines":[],"note":"a${BOM}b"}`,
  ),
  vector(
    'leading-nbsp-is-not-trimmed',
    'A non-breaking space in front of the document. It is NOT in the trim set, so it stays and the document is refused as unparseable. An implementation that used JavaScript String.trim, Java String.strip or .NET Trim would strip it and produce the plain hash instead.',
    `${NBSP}${DOC}`,
    { accepted: false, reject: 'not-a-json-object' },
  ),
  vector(
    'line-separator-is-not-a-line-ending',
    'U+2028 is a line separator to some editors and is not touched here: only CRLF and CR become LF.',
    '{"version":1,"machines":[],"note":"a\\u2028b"}',
  ),
  vector(
    'internal-whitespace-is-significant',
    'The baseline document with its indentation removed. A different byte string is a different document, so this hash differs from plain-no-trailing-newline. Reformatting a config is a real change and pushes.',
    JSON.stringify(JSON.parse(DOC)),
  ),
  vector(
    'single-newline-only',
    'A file containing nothing but a newline. Canonicalises to one newline, which is not a JSON object, so it is refused before it is ever stored.',
    '\n',
    { accepted: false, reject: 'empty' },
  ),
  vector(
    'empty',
    'No bytes at all, which is what a failed upload leaves behind. Refused, never stored as an empty document.',
    '',
    { accepted: false, reject: 'empty' },
  ),
  vector(
    'invalid-utf8',
    'A lone 0x80 continuation byte, as a truncated multi-byte transfer produces. Not valid UTF-8: the document is refused, and no hash exists for it. An implementation that decodes lossily would hash a document full of U+FFFD and agree with nobody.',
    Buffer.from([0x7b, 0x22, 0x76, 0x80, 0x22, 0x7d]),
    { accepted: false, reject: 'invalid-utf8' },
  ),
];

const names = new Set();
for (const entry of vectors) {
  if (names.has(entry.name)) throw new Error(`duplicate vector name: ${entry.name}`);
  names.add(entry.name);
}

const document = {
  $schema: './schemas/hash-vectors.schema.json',
  contract: 3,
  algorithm: 'sha256',
  encoding: 'lowercase hex of the sha256 of the canonical UTF-8 bytes',
  steps: [
    'Decode the bytes as UTF-8. Refuse anything that is not valid UTF-8; it has no hash.',
    'Strip one leading U+FEFF, if and only if it is the very first code point.',
    'Replace every CRLF with LF, then every remaining lone CR with LF.',
    'Remove code points U+0009, U+000A, U+000B, U+000C, U+000D and U+0020 from both ends, and nothing else. Do not use the language runtime’s own trim.',
    'Append exactly one LF.',
    'Encode as UTF-8. The hash is the lowercase hex sha256 of those bytes, and those bytes are what is stored on disk.',
  ],
  trimCodePoints: ['U+0009', 'U+000A', 'U+000B', 'U+000C', 'U+000D', 'U+0020'],
  notes: [
    'inputBase64 is base64 of the raw input bytes; canonicalBase64 is base64 of the bytes after the steps above. Both are base64 because a JSON string cannot carry a lone CR or an invalid byte.',
    'accepted is whether the agent stores the document at all. A vector with accepted false still has a hash when its bytes are valid UTF-8, because canonicalisation happens before the document is inspected; the store then refuses it and the hash is never published.',
    'A client that implements this rule correctly reproduces canonicalBase64 and sha256 for every vector, and refuses every vector whose sha256 is null.',
  ],
  vectors,
};

fs.writeFileSync(OUT, `${JSON.stringify(document, null, 2)}\n`, 'utf8');
process.stdout.write(`wrote ${path.relative(process.cwd(), OUT)} with ${vectors.length} vectors\n`);
