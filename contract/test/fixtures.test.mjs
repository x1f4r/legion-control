// The fixtures, and the proof that checking them means something.
//
// The first test runs contract/validate.mjs and expects it to pass. On its own
// that proves nothing: a validator that reported no problems whatever it was
// given would pass it too. So the rest of this file copies the whole contract
// directory, breaks one thing in it, and asserts that the check FAILS — once per
// defect, each one a mistake somebody could really make.
//
// A defect that is not caught here is a defect that reaches a client.

import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { createHash } from 'node:crypto';
import { CONTRACT, defect, readJson, runValidate, writeJson } from './support.mjs';

test('the contract as committed passes its own check', () => {
  const result = runValidate(CONTRACT);
  assert.equal(result.ok, true, result.output);
  assert.match(result.output, /everything holds/);
  assert.match(result.output, /35\/35 reason codes shown/);
});

test('the index and the directory describe each other exactly', () => {
  const index = readJson(path.join(CONTRACT, 'fixtures', 'index.json'));
  const onDisk = fs.readdirSync(path.join(CONTRACT, 'fixtures'))
    .filter((name) => name.endsWith('.json') && name !== 'index.json')
    .sort();
  assert.deepEqual(index.fixtures.map((entry) => entry.file).sort(), onDisk);
  for (const entry of index.fixtures) {
    assert.ok(entry.description.length > 30, `${entry.file} has no description worth reading`);
    assert.ok(['reply', 'document'].includes(entry.kind));
    assert.ok(['valid', 'invalid'].includes(entry.expect));
    assert.ok(fs.existsSync(path.join(CONTRACT, 'schemas', entry.schema)), `${entry.file} names a missing schema`);
  }
});

test('every peer-amendment fixture the client workers were promised is present', () => {
  const index = readJson(path.join(CONTRACT, 'fixtures', 'index.json'));
  const byVariant = new Map(index.fixtures.map((entry) => [`${entry.command ?? entry.schema}/${entry.variant}`, entry]));
  for (const required of [
    'config set/stored', 'config set/noop', 'config set/fast-forward', 'config set/stale-revision',
    'config set/conflict-divergent', 'config set/conflict-other-id', 'config set/replaced',
    'config meta/meta-with-lineage', 'run/wol-ran',
    'controller-document.schema.json/sites-helpers',
    'controller-document.schema.json/invalid-helper-cycle',
    'bindings.schema.json/desktop-self',
  ]) {
    assert.ok(byVariant.has(required), `no fixture for ${required}`);
  }
});

test('the setup document and the hash reported for it agree, in bytes', () => {
  // The cross-language check every client uses: canonicalise this file in
  // Swift, Kotlin or C# and you must land on the hash the config replies carry.
  const bytes = fs.readFileSync(path.join(CONTRACT, 'fixtures', 'controller-document.sites-helpers.json'));
  const expected = createHash('sha256').update(bytes).digest('hex');
  assert.equal(readJson(path.join(CONTRACT, 'fixtures', 'config.read.json')).hash, expected);
  assert.equal(readJson(path.join(CONTRACT, 'fixtures', 'config.meta.json')).meta.hash, expected);
  assert.equal(readJson(path.join(CONTRACT, 'fixtures', 'config.read.json')).bytes, bytes.length);
  // The file is already in canonical form, so its raw sha256 is its canonical
  // hash. If that stops being true this assertion is the one that notices.
  assert.ok(bytes[bytes.length - 1] === 0x0a && bytes[bytes.length - 2] !== 0x0a);
  assert.ok(!bytes.includes(0x0d));
});

defect('a setup document edited without updating the hash reported for it',
  /would be checking against nothing/, (dir) => {
    const file = path.join(dir, 'fixtures', 'controller-document.sites-helpers.json');
    const data = readJson(file);
    data.machines[0].name = 'Renamed';
    fs.writeFileSync(file, `${JSON.stringify(data, null, 2)}\n`);
  });

// --- the check bites --------------------------------------------------------

defect('a status whose 2.x compatibility block drifts from its first service',
  /t3\.installed is/, (dir) => {
    const file = path.join(dir, 'fixtures', 'status.full.json');
    const data = readJson(file);
    data.t3.installed = '9.9.9';
    writeJson(file, data);
  });

defect('a busy gate that does not know and does not block',
  /fail-open|expected true/, (dir) => {
    const file = path.join(dir, 'fixtures', 'status.full.json');
    const data = readJson(file);
    data.services[2].busy.busy = false;
    writeJson(file, data);
  });

defect('a reason code outside the closed enum',
  /is not one of/, (dir) => {
    const file = path.join(dir, 'fixtures', 'update.deferred-busy.json');
    const data = readJson(file);
    data.reasonCode = 'nearly-busy';
    writeJson(file, data);
  });

defect('a conflict that does not name what holds the lock',
  /missing required key "conflict"/, (dir) => {
    const file = path.join(dir, 'fixtures', 'update.conflict.json');
    const data = readJson(file);
    delete data.conflict;
    writeJson(file, data);
  });

defect('a queued operation with no expiry',
  /expiresAt|allowed shapes/, (dir) => {
    const file = path.join(dir, 'fixtures', 'update.queued.json');
    const data = readJson(file);
    data.op.expiresAt = null;
    writeJson(file, data);
  });

defect('an operation whose timestamps run backwards',
  /runs backwards/, (dir) => {
    const file = path.join(dir, 'fixtures', 'op.finished-updated.json');
    const data = readJson(file);
    data.op.requestedAt = '2026-09-06T00:00:00.000Z';
    writeJson(file, data);
  });

defect('a failed result that will not say why',
  /will not say why/, (dir) => {
    const file = path.join(dir, 'fixtures', 'op.finished-interrupted.json');
    const data = readJson(file);
    data.op.result.reasonCode = null;
    writeJson(file, data);
  });

defect('self-update claiming it installed without a verified signature',
  /signatureVerified/, (dir) => {
    const file = path.join(dir, 'fixtures', 'self-update.installed.json');
    const data = readJson(file);
    data.manifest.signatureVerified = false;
    writeJson(file, data);
  });

defect('a config-set refusal that does not say what the machine still holds',
  /missing required key "current"/, (dir) => {
    const file = path.join(dir, 'fixtures', 'config.set.conflict-divergent.json');
    const data = readJson(file);
    delete data.current;
    writeJson(file, data);
  });

defect('a divergent refusal that forgot to say it was divergent',
  /missing required key "divergent"/, (dir) => {
    const file = path.join(dir, 'fixtures', 'config.set.conflict-divergent.json');
    const data = readJson(file);
    delete data.divergent;
    writeJson(file, data);
  });

defect('a controller meta with no lineage, which makes descent undecidable',
  /missing required key "lineage"/, (dir) => {
    const file = path.join(dir, 'fixtures', 'config.meta.json');
    const data = readJson(file);
    delete data.meta.lineage;
    writeJson(file, data);
  });

defect('a wake helper cycle in a document the index calls valid',
  /wake helper cycle/, (dir) => {
    const file = path.join(dir, 'fixtures', 'controller-document.sites-helpers.json');
    const data = readJson(file);
    const pi = data.machines.find((machine) => machine.id === 'pi');
    pi.wake = { mac: 'DE:AD:BE:EF:00:01', helper: { machine: 'legion', action: 'wake-pi' }, helpers: [{ machine: 'legion', action: 'wake-pi' }] };
    writeJson(file, data);
  });

defect('an invalid fixture that nothing actually rejects',
  /the index expects this to be refused/, (dir) => {
    // Repair the broken document but leave the index calling it invalid. If the
    // rule it was written for ever disappears, this is what notices.
    const file = path.join(dir, 'fixtures', 'controller-document.invalid-self-helper.json');
    writeJson(file, { version: 1, machines: [{ id: 'tower', name: 'Tower' }] });
  });

defect('a 2.x fixture that claims to speak contract 3',
  /no value is allowed here/, (dir) => {
    const file = path.join(dir, 'fixtures', 'legacy-2x.status.json');
    const data = readJson(file);
    data.contract = 3;
    writeJson(file, data);
  });

defect('a hash vector with the wrong hash',
  /hash is .*the vector says/, (dir) => {
    const file = path.join(dir, 'hash-vectors.json');
    const data = readJson(file);
    data.vectors[3].sha256 = '0'.repeat(64);
    writeJson(file, data);
  });

defect('canonical bytes that do not follow from the input',
  /canonical bytes disagree/, (dir) => {
    const file = path.join(dir, 'hash-vectors.json');
    const data = readJson(file);
    data.vectors[6].canonicalBase64 = data.vectors[12].canonicalBase64;
    writeJson(file, data);
  });

defect('a reason code no fixture ever shows',
  /no fixture shows these reason codes/, (dir) => {
    const file = path.join(dir, 'schemas', 'common.schema.json');
    const data = readJson(file);
    data.$defs.reasonCode.enum.push('newly-invented-code');
    writeJson(file, data);
  });

defect('a schema $ref that resolves to nothing',
  /does not resolve/, (dir) => {
    const file = path.join(dir, 'schemas', 'common.schema.json');
    const data = readJson(file);
    data.$defs.opRecord.properties.phase = { $ref: '#/$defs/nonexistent' };
    writeJson(file, data);
  });

defect('a mistyped keyword, which would make a schema accept anything',
  /unknown keyword "itmes"/, (dir) => {
    const file = path.join(dir, 'schemas', 'status.schema.json');
    const data = readJson(file);
    data.properties.services.itmes = { type: 'object' };
    writeJson(file, data);
  });

defect('a fixture on disk that the index does not list',
  /on disk and not in the index/, (dir) => {
    const file = path.join(dir, 'fixtures', 'index.json');
    const data = readJson(file);
    data.fixtures = data.fixtures.filter((entry) => entry.file !== 'doctor.ok.json');
    writeJson(file, data);
  });

defect('an index entry that disagrees with the fixture it names',
  /index says ok false, the fixture says true/, (dir) => {
    const file = path.join(dir, 'fixtures', 'index.json');
    const data = readJson(file);
    data.fixtures.find((entry) => entry.file === 'status.full.json').ok = false;
    writeJson(file, data);
  });

// --- the additions of agent 3.1.0 -------------------------------------------

test('every reply fixture of a mutating command states its outcome, and no other does', () => {
  const index = readJson(path.join(CONTRACT, 'fixtures', 'index.json'));
  const outcomes = new Set();
  for (const entry of index.fixtures) {
    if (entry.kind !== 'reply') {
      assert.equal(entry.outcome, null, `${entry.file} is a document and has no outcome`);
      continue;
    }
    const data = readJson(path.join(CONTRACT, 'fixtures', entry.file));
    assert.equal(entry.outcome, data.outcome ?? null, `${entry.file}: the index and the fixture disagree about outcome`);
    if (data.outcome) outcomes.add(data.outcome);
  }
  assert.deepEqual([...outcomes].sort(), ['deferred', 'failed', 'pending', 'succeeded', 'unknown']);
});

test('config.read.json carries the stored text, and the fallback fixture carries none', () => {
  const read = readJson(path.join(CONTRACT, 'fixtures', 'config.read.json'));
  const text = fs.readFileSync(path.join(CONTRACT, 'fixtures', 'controller-document.sites-helpers.json'), 'utf8');
  assert.equal(read.raw, text);
  assert.equal(Buffer.byteLength(read.raw, 'utf8'), read.bytes);
  assert.equal(createHash('sha256').update(read.raw).digest('hex'), read.hash);
  const older = readJson(path.join(CONTRACT, 'fixtures', 'config.read-without-raw.json'));
  assert.equal('raw' in older, false);
  assert.equal(older.agentVersion, '3.0.1');
});

defect('a raw that is not the document the reply reports the hash of',
  /the sha256 of raw is/, (dir) => {
    const file = path.join(dir, 'fixtures', 'config.read.json');
    const data = readJson(file);
    data.raw = data.raw.replace('"Home setup"', '"Home setup!"');
    data.bytes = Buffer.byteLength(data.raw, 'utf8');
    writeJson(file, data);
  });

defect('a raw beside a null controller',
  /raw: expected null, got string/, (dir) => {
    const file = path.join(dir, 'fixtures', 'config.read-empty.json');
    const data = readJson(file);
    data.raw = '{"version":1,"machines":[]}\n';
    writeJson(file, data);
  });

defect('an outcome more optimistic than the reducer',
  /the reducer of section 5\.6 gives "unknown"/, (dir) => {
    const file = path.join(dir, 'fixtures', 'run.timed-out.json');
    const data = readJson(file);
    data.outcome = 'failed';
    writeJson(file, data);
  });

defect('a record whose outcome and result disagree',
  /a finished record states the same outcome in both places/, (dir) => {
    const file = path.join(dir, 'fixtures', 'op.finished-rolled-back.json');
    const data = readJson(file);
    data.op.outcome = 'unknown';
    writeJson(file, data);
  });

defect('a mutating reply that states no outcome',
  /carries outcome \(section 5\.6\), and this one does not/, (dir) => {
    const file = path.join(dir, 'fixtures', 'update.deferred-busy.json');
    const data = readJson(file);
    delete data.outcome;
    writeJson(file, data);
    const indexFile = path.join(dir, 'fixtures', 'index.json');
    const index = readJson(indexFile);
    index.fixtures.find((entry) => entry.file === 'update.deferred-busy.json').outcome = null;
    writeJson(indexFile, index);
  });

defect('an outcome on a reply that changes nothing',
  /changes nothing, so it carries no outcome/, (dir) => {
    const file = path.join(dir, 'fixtures', 'busy.idle.json');
    const data = readJson(file);
    data.outcome = 'succeeded';
    writeJson(file, data);
    const indexFile = path.join(dir, 'fixtures', 'index.json');
    const index = readJson(indexFile);
    index.fixtures.find((entry) => entry.file === 'busy.idle.json').outcome = 'succeeded';
    writeJson(indexFile, index);
  });

defect('an index exit code that rule 7 does not give',
  /index says exit code 1, and rule 7 gives 0/, (dir) => {
    const file = path.join(dir, 'fixtures', 'index.json');
    const data = readJson(file);
    data.fixtures.find((entry) => entry.file === 'update.replayed.json').exitCode = 1;
    writeJson(file, data);
  });

defect('an index entry naming a schema the command is not read with',
  /a config reply of this shape is read with config-read\.schema\.json/, (dir) => {
    const file = path.join(dir, 'fixtures', 'index.json');
    const data = readJson(file);
    data.fixtures.find((entry) => entry.file === 'config.meta.json').command = 'config';
    writeJson(file, data);
  });

defect('a cycle child with no record that was not a deferral',
  /expected "deferred"/, (dir) => {
    const file = path.join(dir, 'fixtures', 'cycle.cycled.json');
    const data = readJson(file);
    data.children[0].opId = null;
    data.op.children[0].opId = null;
    writeJson(file, data);
  });

defect('launchChanges on a refusal that is not restricted',
  /no value is allowed here/, (dir) => {
    const file = path.join(dir, 'fixtures', 'config.set.conflict-divergent.json');
    const data = readJson(file);
    data.launchChanges = ['machines[tower].systems[linux]'];
    writeJson(file, data);
  });

defect('a launch path in a form no client can find',
  /does not match/, (dir) => {
    const file = path.join(dir, 'fixtures', 'config.set.restricted-launch.json');
    const data = readJson(file);
    data.launchChanges = ['machines.tower.systems.linux'];
    writeJson(file, data);
  });

defect('a guard fixture that the host guard no longer refuses',
  /the index expects this to be refused/, (dir) => {
    const file = path.join(dir, 'fixtures', 'controller-document.invalid-host.json');
    const data = readJson(file);
    data.machines[0].endpoints[0].host = 'desk-tower.local';
    writeJson(file, data);
  });

defect('busy counts that do not follow from the services',
  /busy counts say 3\/1, the services say 2\/1/, (dir) => {
    const file = path.join(dir, 'fixtures', 'busy.several-services.json');
    const data = readJson(file);
    data.monitoredServices = 3;
    writeJson(file, data);
  });

defect('a machine with no services that reports a service nobody described',
  /a machine with no services answers monitored true/, (dir) => {
    const file = path.join(dir, 'fixtures', 'busy.no-services.json');
    const data = readJson(file);
    Object.assign(data, { evidence: 'command', reason: 'idle' });
    writeJson(file, data);
  });
