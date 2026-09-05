// Detached signatures keep release trust independent from download hosting and file names.
import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';
import { fileURLToPath } from 'node:url';

export const MAX_MANIFEST_BYTES = 256 * 1024;
export function readManifestBytes(file) {
  const bytes = fs.readFileSync(file);
  if (!bytes.length || bytes.length > MAX_MANIFEST_BYTES) throw new Error('Release manifest is empty or exceeds 256 KiB.');
  const document = JSON.parse(bytes.toString('utf8'));
  if (!document || typeof document !== 'object' || Array.isArray(document)) throw new Error('Release manifest must be an object.');
  return bytes;
}
export function createSigningKey(privateFile, publicFile) {
  if (fs.existsSync(privateFile) || fs.existsSync(publicFile)) throw new Error('Signing keys already exist; refusing to replace them.');
  const pair = crypto.generateKeyPairSync('ed25519');
  fs.mkdirSync(path.dirname(privateFile), { recursive: true, mode: 0o700 });
  fs.mkdirSync(path.dirname(publicFile), { recursive: true });
  fs.writeFileSync(privateFile, pair.privateKey.export({ type: 'pkcs8', format: 'pem' }), { mode: 0o600, flag: 'wx' });
  fs.writeFileSync(publicFile, pair.publicKey.export({ type: 'spki', format: 'pem' }), { mode: 0o644, flag: 'wx' });
}
export function signManifest(manifestFile, privateFile, signatureFile) {
  const bytes = readManifestBytes(manifestFile);
  const key = crypto.createPrivateKey(fs.readFileSync(privateFile));
  if (key.asymmetricKeyType !== 'ed25519') throw new Error('Release signing requires an Ed25519 key.');
  const signature = crypto.sign(null, bytes, key);
  fs.writeFileSync(signatureFile, signature.toString('base64') + '\n');
}
export function verifyManifest(manifestFile, signatureFile, publicFile) {
  const bytes = readManifestBytes(manifestFile);
  const encoded = fs.readFileSync(signatureFile, 'utf8').trim();
  if (!/^[A-Za-z0-9+/]{86}==$/.test(encoded)) throw new Error('Malformed release signature.');
  const signature = Buffer.from(encoded, 'base64');
  const key = crypto.createPublicKey(fs.readFileSync(publicFile));
  if (key.asymmetricKeyType !== 'ed25519' || !crypto.verify(null, bytes, key, signature)) throw new Error('Release signature does not match the trusted key.');
  return JSON.parse(bytes.toString('utf8'));
}
export async function artifactDigest(file) {
  const hash = crypto.createHash('sha256');
  let size = 0;
  for await (const chunk of fs.createReadStream(file)) { hash.update(chunk); size += chunk.length; }
  return { sha256: hash.digest('hex'), size };
}
export async function verifyArtifact(file, expected) {
  if (!expected || !Number.isSafeInteger(expected.size) || expected.size < 0 || !/^[a-f0-9]{64}$/.test(expected.sha256)) throw new Error('Invalid artifact digest or size.');
  const actual = await artifactDigest(file);
  if (actual.size !== expected.size || actual.sha256 !== expected.sha256) throw new Error('Artifact bytes do not match the signed manifest.');
  return true;
}
if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    const [verb, ...args] = process.argv.slice(2);
    if (verb === 'keygen' && args.length === 2) createSigningKey(...args);
    else if (verb === 'sign' && args.length === 3) signManifest(...args);
    else if (verb === 'verify' && args.length === 3) verifyManifest(...args);
    else throw new Error('Usage: release-integrity.mjs keygen PRIVATE PUBLIC | sign MANIFEST PRIVATE SIGNATURE | verify MANIFEST SIGNATURE PUBLIC');
    console.log('Release integrity check completed.');
  } catch (error) { console.error(error.message); process.exitCode = 1; }
}
