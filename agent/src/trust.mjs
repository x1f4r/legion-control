// The one key this agent trusts, and the only one.
//
// Every signed thing the agent will ever accept — a release manifest, an agent
// bundle pushed by a client — is verified against this key and nothing else.
// It is compiled in rather than configured, and that is the point:
//
// Shared controller setup can be published by a restricted SSH key. Publishing
// topology must never grant authority to replace the executable code. The release
// key is therefore part of the installed program, separate from shared topology
// and local machine configuration, with no runtime override.
//
// The value is the exact contents of contract/release-public-key.pem. Rotating
// it means shipping a release signed by the OLD key that carries the new one; a
// key that is lost without a backup cannot be rotated at all, and every install
// then has to be replaced by hand.

import crypto from 'node:crypto';

/** contract/release-public-key.pem, verbatim. SPKI PEM, Ed25519. */
export const RELEASE_PUBLIC_KEY_PEM = [
  '-----BEGIN PUBLIC KEY-----',
  'MCowBQYDK2VwAyEAB8q1DNamFF0ShMi6Wzps/0UIGEkLmHDDvQqOvuv98zg=',
  '-----END PUBLIC KEY-----',
].join('\n');

/** Ed25519 signatures are 64 bytes, which is 88 base64 characters with padding. */
const SIGNATURE_PATTERN = /^[A-Za-z0-9+/]{86}==$/;

let cachedKey = null;

function publicKey() {
  if (!cachedKey) {
    cachedKey = crypto.createPublicKey(RELEASE_PUBLIC_KEY_PEM);
    if (cachedKey.asymmetricKeyType !== 'ed25519') {
      throw new Error('the compiled release key is not an Ed25519 key');
    }
  }
  return cachedKey;
}

/**
 * Verify a detached signature over exact bytes.
 *
 * `signature` is the contents of the .sig file: base64 of the raw 64 bytes, with
 * or without a trailing newline. Returns a reason rather than throwing, because
 * every caller has to report the failure rather than crash on it.
 */
export function verifyDetachedSignature(bytes, signature) {
  const encoded = String(signature ?? '').trim();
  if (!SIGNATURE_PATTERN.test(encoded)) {
    return { ok: false, error: 'the signature is not 64 base64-encoded bytes' };
  }
  let ok = false;
  try {
    ok = crypto.verify(null, Buffer.isBuffer(bytes) ? bytes : Buffer.from(bytes), publicKey(), Buffer.from(encoded, 'base64'));
  } catch (err) {
    return { ok: false, error: `the signature could not be checked: ${err.message}` };
  }
  return ok
    ? { ok: true, error: null }
    : { ok: false, error: 'the signature does not match the release key compiled into this agent' };
}

/** A short, stable fingerprint of the trusted key, for doctor and diagnostics. */
export function trustFingerprint() {
  const der = publicKey().export({ type: 'spki', format: 'der' });
  return crypto.createHash('sha256').update(der).digest('hex').slice(0, 16);
}
