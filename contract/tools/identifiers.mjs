// The identifier grammars, as a function every implementation must agree with.
//
// The grammars themselves are not written here. They are read from
// contract/schemas/common.schema.json at import, because that file is what the
// fixtures are validated against and what a client's decoder is checked by; a
// second copy of the patterns in this file would be one more place for them to
// drift, which is how two clients came to accept setup ids the agent refuses.
// What this file adds is the matching rule every implementation shares, which
// a pattern on its own does not carry: the whole input must match, classes are
// ASCII, and a line feed or carriage return is refused before any pattern is
// applied, so that a .NET or ICU `$` that also matches before a final newline
// can never let one through.

import fs from 'node:fs';

const COMMON = JSON.parse(fs.readFileSync(new URL('../schemas/common.schema.json', import.meta.url), 'utf8'));

export const FAMILIES = ['identifiers'];

/** One group per grammar, named after its $defs entry in common.schema.json. */
export const GROUPS = ['token', 'configuredId', 'setupId', 'opId', 'duration', 'clockTime', 'sha256'];

const PATTERNS = new Map(GROUPS.map((group) => {
  const definition = COMMON.$defs[group];
  if (!definition || typeof definition.pattern !== 'string' || definition.type !== 'string') {
    throw new Error(`common.schema.json#/$defs/${group} is not a string pattern, so the identifiers family cannot pin it`);
  }
  // Every constraint of the definition has to be one this function applies.
  // A length limit added to the schema would otherwise be enforced by the
  // fixtures and silently ignored by the vectors.
  const unread = Object.keys(definition).filter((key) => !['type', 'pattern', 'description'].includes(key));
  if (unread.length > 0) {
    throw new Error(`common.schema.json#/$defs/${group} has ${unread.join(', ')}, which identifiers.mjs does not apply`);
  }
  return [group, new RegExp(definition.pattern, 'u')];
}));

export function run(group, input) {
  const pattern = PATTERNS.get(group);
  if (!pattern) throw new Error(`the identifiers family has no group "${group}"`);
  const { value } = input;
  const valid = typeof value === 'string' && !/[\n\r]/.test(value) && pattern.test(value);
  return { valid };
}
