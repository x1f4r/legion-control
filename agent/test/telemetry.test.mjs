import assert from 'node:assert/strict';
import test from 'node:test';
import { deadline, normalizeConfig } from '../src/config.mjs';
import { collectTelemetry } from '../src/probes/telemetry.mjs';
import { buildStatus } from '../src/status.mjs';
import { withHome } from './helpers.mjs';

const probe = (id, code = 'console.log(52.5)') => ({ id, command: [process.execPath, '-e', code] });
const document = (telemetry) => ({ configVersion: 3, system: { id: 'test', name: 'Test' }, services: [], ...(telemetry === undefined ? {} : { telemetry }) });

test('telemetry is absent by default and only explicitly configured probes are reported', async () => {
  await withHome(async () => {
    for (const telemetry of [undefined, null, { probes: [] }]) {
      const status = await buildStatus(normalizeConfig(document(telemetry)), { budgetMs: 1000 });
      assert.equal(Object.hasOwn(status, 'metrics'), false);
      assert.ok(status.agent.capabilities.includes('configured-telemetry'));
    }
    const config = normalizeConfig(document({ probes: [{ ...probe('temperature'), name: 'CPU temperature', unit: '°C' }] }));
    assert.equal(config.ok, true);
    assert.equal(config.config.telemetry.probes[0].timeoutSeconds, 1);
    const status = await buildStatus(config, { budgetMs: 1000 });
    assert.equal(status.metrics.length, 1);
    assert.deepEqual({ ...status.metrics[0], checkedAt: null }, {
      id: 'temperature', name: 'CPU temperature', value: 52.5, unit: '°C', checkedAt: null, error: null,
    });
    assert.ok(Number.isFinite(Date.parse(status.metrics[0].checkedAt)));
  });
});

test('telemetry config rejects invalid commands, limits, duplicates and unknown keys', () => {
  const invalid = [
    { probes: Array.from({ length: 9 }, (_, i) => probe(`probe${i}`)) },
    { probes: [probe('same'), probe('same')] },
    { probes: [{ ...probe('slow'), timeoutSeconds: 3 }] },
    { probes: [{ ...probe('zero'), timeoutSeconds: 0 }] },
    { probes: [{ ...probe('text'), timeoutSeconds: '1' }] },
    { probes: [{ ...probe('wrong'), command: 'echo 5' }] },
    { probes: [{ ...probe('wrong'), command: ['echo', 5] }] },
    { probes: [{ ...probe('wrong'), command: ['echo', '\0'] }] },
    { probes: [{ ...probe('wrong'), name: 'x'.repeat(121) }] },
    { probes: [{ ...probe('wrong'), unit: 'x'.repeat(25) }] },
    { probes: [probe('x'.repeat(65))] },
    { probes: [probe('bad id')] },
    { probes: [{ ...probe('wrong'), automaticScan: true }] },
    { probes: [], discover: true },
  ];
  for (const telemetry of invalid) assert.equal(normalizeConfig(document(telemetry)).ok, false, JSON.stringify(telemetry));
});

test('only finite numeric stdout is a measurement; zero remains a valid measurement', async () => {
  for (const [stdout, expected] of [['0', 0], [' -2.5e1\n', -25], ['', null], ['NaN', null], ['Infinity', null], ['1e999', null], ['42 °C', null], ['true', null], ['2\n3', null]]) {
    const metrics = await collectTelemetry([{ id: 'x', name: 'X', unit: '%', timeoutSeconds: 1, command: ['unused'] }], deadline(1000), {
      runner: async () => ({ ok: true, stdout }),
    });
    assert.equal(metrics[0].value, expected, stdout);
    assert.equal(metrics[0].error === null, expected !== null);
  }
});

test('telemetry shares the status deadline and skipped probes do not execute', async () => {
  await withHome(async () => {
    const loaded = normalizeConfig(document({ probes: Array.from({ length: 8 }, (_, i) => probe(`probe${i}`, 'setTimeout(() => console.log(1), 900)')) }));
    const started = Date.now();
    const status = await buildStatus(loaded, { budgetMs: 100 });
    assert.ok(Date.now() - started < 500, JSON.stringify(status.timing));
    assert.equal(status.timing.partial, true);
    assert.equal(status.metrics.length, 8);
    for (const metric of status.metrics) {
      assert.equal(metric.value, null);
      assert.match(metric.error, /timed out|no time left/);
    }
    assert.equal(status.metrics.filter((metric) => metric.checkedAt !== null).length, 2);
  });
});

test('telemetry runs at most two probes at once', async () => {
  let active = 0;
  let peak = 0;
  const probes = Array.from({ length: 8 }, (_, i) => ({ id: String(i), name: String(i), unit: '', command: ['unused'], timeoutSeconds: 1 }));
  const metrics = await collectTelemetry(probes, deadline(1000), {
    runner: async () => {
      peak = Math.max(peak, ++active);
      await new Promise((resolve) => setTimeout(resolve, 5));
      active -= 1;
      return { ok: true, stdout: '1' };
    },
  });
  assert.equal(peak, 2);
  assert.equal(metrics.length, 8);
});
