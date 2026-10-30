// The shared conformance vectors, as the agent reads them.
//
// contract/vectors/index.json says which groups each consumer runs and how
// many cases it must have run (contract section 14.2). The agent is one of
// those consumers, and the corpus is in the same repository, so a missing file
// is a failure here rather than a reason to skip.

import fs from 'node:fs';
import path from 'node:path';
import { REPO } from './helpers.mjs';

export const CONTRACT = path.join(REPO, 'contract');

const readJson = (file) => JSON.parse(fs.readFileSync(file, 'utf8'));

/** contract/vectors/index.json, or an error that says the tests need it. */
export function vectorIndex() {
  const file = path.join(CONTRACT, 'vectors', 'index.json');
  if (!fs.existsSync(file)) throw new Error(`${file} is missing; the agent tests need the shared corpus`);
  return readJson(file);
}

/**
 * Every group that lists `consumer`, as `{ key, family, group, count, cases }`
 * with `key` written `family/group` and `cases` the ones that name the consumer.
 */
export function groupsFor(consumer, index = vectorIndex()) {
  const groups = [];
  for (const family of index.families) {
    const file = path.join(CONTRACT, family.file);
    if (!fs.existsSync(file)) throw new Error(`${file} is missing; contract/vectors/index.json lists it`);
    const data = readJson(file);
    for (const group of family.groups) {
      const count = group.consumers[consumer];
      if (count === undefined) continue;
      // The hash family is flat: one group, every vector in it, for everyone.
      const cases = family.layout === 'flat'
        ? data.vectors
        : data.groups[group.group].filter((entry) => entry.consumers.includes(consumer));
      groups.push({ key: `${family.family}/${group.group}`, family: family.family, group: group.group, count, cases });
    }
  }
  return groups;
}
