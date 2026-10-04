// Every vector family, in the order contract/vectors/index.json lists them.
//
// A family is added here and nowhere else: make-vectors.mjs builds what this
// list names, writes its file and counts it into the index, and validate.mjs
// then refuses a family whose file, schema, reference or generator it cannot
// find. A case module that exists but is not listed is never generated, which
// is why this list is the one place to look.

import identifiers from './identifiers.cases.mjs';
import launch from './launch.cases.mjs';
import outcome from './outcome.cases.mjs';
import setupAcceptance from './setup-acceptance.cases.mjs';
import setupMerge from './setup-merge.cases.mjs';
import setupReconcile from './setup-reconcile.cases.mjs';
import transport from './transport.cases.mjs';
import wake from './wake.cases.mjs';

/**
 * The hash family keeps the file and the flat layout it had before there was a
 * vector index: contract/hash-vectors.json, one array, each vector its own
 * input. It is described here so the index can list it beside the others, and
 * make-hash-vectors.mjs writes it.
 */
export const HASH_FAMILY = Object.freeze({
  family: 'hash',
  file: 'hash-vectors.json',
  schema: 'hash-vectors.schema.json',
  layout: 'flat',
  section: '12',
  reference: 'tools/canonical.mjs',
  generator: 'tools/make-hash-vectors.mjs',
  group: 'vectors',
  consumers: Object.freeze(['agent', 'mac', 'desktop', 'android']),
});

/** The case modules of the families in the `groups` layout, each written to vectors/<family>.json. */
export const CASE_MODULES = Object.freeze([identifiers, outcome, transport, setupAcceptance, launch, setupReconcile, setupMerge, wake]);
