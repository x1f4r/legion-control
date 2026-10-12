// The identifier grammars of v3.md section 1, rule 5, as cases.
//
// Every case is one string and whether it is valid. The interesting ones are
// the strings a regular expression engine disagrees about: a trailing newline
// that .NET's `$` lets through, a digit that `\d` matches in Kotlin and .NET but
// is not 0-9, a letter that a Unicode-aware class accepts. The agent refuses all
// of them, so a client that accepts one sends a request that can only ever come
// back as bad-argument.

const EVERYONE = ['agent', 'mac', 'desktop', 'android'];

const valid = (name, value, purpose, tags = []) => ({ name, purpose, tags, input: { value }, want: { valid: true } });
const invalid = (name, value, purpose, tags = []) => ({ name, purpose, tags, input: { value }, want: { valid: false } });

const UUID = '6f1c2a84-3b7d-4e19-9a52-0c8f4d61b7a3';
const SHA = '28cab054f9ba19fa2c630e50a80bd8469ef11d7f3a50e0a171c2db62467186ce';

export default {
  family: 'identifiers',
  section: '1',
  reference: 'tools/identifiers.mjs',
  notes: [
    'Each group pins one $defs entry of schemas/common.schema.json, and its name is that entry: token, configuredId, setupId, opId, duration, clockTime, sha256.',
    'Match the whole input with ASCII classes only, and refuse U+000A and U+000D before any pattern is applied: \\A...\\z in .NET and ICU, Regex.matches() in Kotlin, ^...$ with the u flag in JavaScript.',
    'Every value is a JSON string; the newline cases carry a real U+000A or U+000D, not a backslash and a letter.',
  ],
  consumers: {
    token: EVERYONE,
    configuredId: EVERYONE,
    setupId: EVERYONE,
    opId: EVERYONE,
    duration: EVERYONE,
    clockTime: EVERYONE,
    sha256: EVERYONE,
  },
  groups: {
    token: [
      valid('token-word', 'demo', 'A plain word, the commonest argument a client sends.'),
      valid('token-mixed-punctuation', 'Demo_2.1-beta', 'Upper case, digits, underscore, dot and hyphen after the first character are all inside the grammar.'),
      valid('token-colon', 'install:abc', 'A colon after the first character is allowed; self-update operation targets look like this.'),
      valid('token-windows-path', 'C:\\Users\\me', 'A colon and backslashes are both in the generic grammar, so a short Windows path is one token.'),
      invalid('token-empty', '', 'The empty string is never a token: the grammar needs a leading letter or digit.'),
      invalid('token-leading-dash', '-demo', 'A leading hyphen reads as an option to whatever receives it, so the first character excludes it.', ['shell-metacharacter']),
      invalid('token-leading-dot', '.demo', 'A leading dot is refused like a leading hyphen: the first character is a letter or digit only.'),
      invalid('token-space', 'de mo', 'A space splits one argument into two in every shell, so it is outside the grammar.', ['shell-metacharacter']),
      invalid('token-semicolon', 'demo;id', 'A semicolon ends a command in POSIX shells, so it never appears in a token.', ['shell-metacharacter']),
      invalid('token-dollar', 'demo$x', 'A dollar sign expands in POSIX shells and PowerShell, so it never appears in a token.', ['shell-metacharacter']),
      invalid('token-non-ascii-letter', 'démo', 'é is a letter to a Unicode-aware class such as \\w; the grammar is ASCII and refuses it.', ['ascii']),
      invalid('token-trailing-lf', 'demo\n', 'A trailing line feed. In .NET and ICU $ also matches before a final newline, so the input is refused before any pattern runs.', ['newline']),
      invalid('token-trailing-crlf', 'demo\r\n', 'A trailing CRLF, checked apart from the LF case because an implementation that strips one of the two lets the other through.', ['newline']),
      invalid('token-line-separator', 'demo\u2028', 'U+2028 is a line terminator to some engines and an ordinary character to others; it is simply not in the grammar.', ['newline']),
      invalid('token-arabic-indic-digit', '\u0663demo', 'U+0663 ARABIC-INDIC DIGIT THREE matches \\d in .NET and Kotlin; the grammar means 0-9 and nothing else.', ['ascii']),
    ],
    configuredId: [
      valid('configured-id-word', 'demo', 'A service, action or boot target id: a plain word.'),
      valid('configured-id-mixed-punctuation', 'Demo_2.1-beta', 'Upper case, digits, underscore, dot and hyphen are the whole of what follows the first character.'),
      valid('configured-id-short', 't3', 'Two characters, a letter and a digit: short ids are ordinary ids.'),
      invalid('configured-id-colon', 'demo:blue', 'A colon is in the generic token grammar but not in a configured id, which the agent matches against config.mjs ID_PATTERN.'),
      invalid('configured-id-slash', 'demo/path', 'A slash is a path, not an id; the token grammar would accept it and the agent refuses it.'),
      invalid('configured-id-at', 'demo@host', 'An at sign is a user and host, not an id; the token grammar would accept it and the agent refuses it.'),
      invalid('configured-id-leading-dash', '-demo', 'A leading hyphen would read as an option, so a configured id starts with a letter or digit.', ['shell-metacharacter']),
      invalid('configured-id-empty', '', 'The empty string names nothing and is refused.'),
      invalid('configured-id-trailing-lf', 'demo\n', 'A trailing line feed is refused before the pattern runs, whatever the engine does with $.', ['newline']),
      invalid('configured-id-non-ascii-letter', 'démo', 'é is not an ASCII letter, and configured ids are ASCII.', ['ascii']),
    ],
    setupId: [
      valid('setup-id-one-character', 'a', 'One letter is a whole setup id: the grammar allows zero to 63 characters after the first.', ['finding:c10']),
      valid('setup-id-colon-and-hyphen', 'setup:desktop-1', 'A colon and a hyphen, as ids written by hand tend to have.', ['finding:c10']),
      valid('setup-id-all-punctuation', 'a.b:c_d-e', 'Every punctuation character the grammar allows, once each.', ['finding:c10']),
      valid('setup-id-generated', 'setup-3f9c2a71-8d04-4e6b-9a53-7c21b0e5d846', 'The shape a client generates: a prefix and a UUID.', ['finding:c10']),
      valid('setup-id-64-characters', 'a'.repeat(64), 'Exactly 64 characters, the longest id there is.', ['finding:c10']),
      invalid('setup-id-65-characters', 'a'.repeat(65), 'One character over the limit. The generic token grammar has no limit and accepts it; the agent refuses it.', ['finding:c10']),
      invalid('setup-id-leading-dash', '-a', 'A leading hyphen is refused: the first character is a letter or digit.', ['finding:c10']),
      invalid('setup-id-slash', 'a/b', 'A slash is in the generic token grammar and not in a setup id; desktop and Android accepted it and the agent refused the push.', ['finding:c10']),
      invalid('setup-id-at', 'a@b', 'An at sign is in the generic token grammar and not in a setup id; desktop and Android accepted it and the agent refused the push.', ['finding:c10']),
      invalid('setup-id-empty', '', 'An empty setup id identifies nothing and is refused.', ['finding:c10']),
      invalid('setup-id-trailing-lf', 'a\n', 'A trailing line feed is refused before the pattern runs.', ['finding:c10', 'newline']),
    ],
    opId: [
      valid('op-id-uuid', UUID, 'A lowercase UUIDv4, the id every client generates for --op.'),
      valid('op-id-eight-characters', 'abcdefgh', 'Eight characters, the shortest id the grammar allows.'),
      valid('op-id-64-characters', 'a'.repeat(64), 'Sixty-four characters, the longest id the grammar allows.'),
      invalid('op-id-seven-characters', 'abcdefg', 'Seven characters, one short of the minimum.'),
      invalid('op-id-65-characters', 'a'.repeat(65), 'Sixty-five characters, one over the maximum.'),
      invalid('op-id-uppercase-uuid', UUID.toUpperCase(), 'An upper-case UUID. Operation ids are compared as strings and the grammar is lower case only.'),
      invalid('op-id-underscore', 'abc_defg', 'An underscore is not in [a-z0-9-].'),
      invalid('op-id-trailing-lf', 'abcdefgh\n', 'A trailing line feed is refused before the pattern runs.', ['newline']),
      invalid('op-id-non-ascii-letter', 'abcdéfgh', 'é is not in [a-z]; a case-insensitive or Unicode-aware match would let it through.', ['ascii']),
    ],
    duration: [
      valid('duration-minutes', '30m', 'Thirty minutes: digits then one of m, h or d.'),
      valid('duration-hours', '4h', 'Four hours, the default expiry of a when-idle request.'),
      valid('duration-days', '2d', 'Two days, the largest unit there is.'),
      valid('duration-zero', '0m', 'Zero is a number like any other; whether it makes sense is the command\'s business, not the grammar\'s.'),
      invalid('duration-seconds', '30s', 'Seconds are not a unit here: m, h and d only.'),
      invalid('duration-unit-only', 'm', 'A unit with no number.'),
      invalid('duration-upper-case-unit', '30M', 'Upper-case units are refused; the grammar is case sensitive.'),
      invalid('duration-empty', '', 'The empty string is not a duration.'),
      invalid('duration-arabic-indic-digit', '\u0663m', 'U+0663 matches \\d in .NET and Kotlin; durations are 0-9 only.', ['ascii']),
      invalid('duration-fullwidth-digit', '\uff13m', 'U+FF13 FULLWIDTH DIGIT THREE also matches \\d in some engines; durations are 0-9 only.', ['ascii']),
      invalid('duration-trailing-lf', '30m\n', 'A trailing line feed is refused before the pattern runs.', ['newline']),
      invalid('duration-negative', '-5m', 'A sign is not part of the grammar.'),
    ],
    clockTime: [
      valid('clock-time-midnight', '00:00', 'Midnight, the start of the range.'),
      valid('clock-time-two', '02:00', 'The start of the nightly window in the fixtures.'),
      valid('clock-time-last-minute', '23:59', 'The last minute of the day, the end of the range.'),
      invalid('clock-time-twenty-four', '24:00', 'Hours run 00 to 23; 24:00 is written 00:00.'),
      invalid('clock-time-one-digit-hour', '2:00', 'Hours always have two digits.'),
      invalid('clock-time-sixty-minutes', '02:60', 'Minutes run 00 to 59.'),
      invalid('clock-time-trailing-lf', '02:00\n', 'A trailing line feed is refused before the pattern runs.', ['newline']),
      invalid('clock-time-arabic-indic-digits', '\u0660\u0662:\u0660\u0660', 'Arabic-Indic digits match \\d in .NET and Kotlin; clock times are 0-9 only.', ['ascii']),
      invalid('clock-time-seconds', '02:00:00', 'Seconds are not part of a clock time.'),
    ],
    sha256: [
      valid('sha256-lowercase', SHA, 'Sixty-four lower-case hex digits: every hash on the wire looks like this.'),
      invalid('sha256-uppercase', SHA.toUpperCase(), 'Upper-case hex is the same number and a different string; hashes are compared as strings, so only lower case is valid.'),
      invalid('sha256-63-characters', SHA.slice(0, 63), 'One hex digit short.'),
      invalid('sha256-65-characters', `${SHA}0`, 'One hex digit too many.'),
      invalid('sha256-trailing-lf', `${SHA}\n`, 'A valid hash followed by a line feed is refused before the pattern runs.', ['newline']),
      invalid('sha256-fullwidth-letter', `${SHA.slice(0, 63)}\uff41`, 'U+FF41 FULLWIDTH LATIN SMALL LETTER A matches a case-folding or Unicode-aware class; hex digits are ASCII only.', ['ascii']),
      invalid('sha256-not-hex', `${SHA.slice(0, 63)}g`, 'g is a letter and not a hex digit.'),
    ],
  },
};
