// Only explicitly configured, read-only numeric probes run during status.
// Measurements are ephemeral; history is a separate client preference.

import { describeFailure, mapWithLimit, runArgvAsync } from '../config.mjs';

export async function collectTelemetry(probes, clock, { runner = runArgvAsync } = {}) {
  return mapWithLimit(probes, 2, async (probe) => {
    const metric = { id: probe.id, name: probe.name, value: null, unit: probe.unit, checkedAt: null, error: null };
    const timeoutMs = clock.slice(probe.timeoutSeconds * 1000);
    if (timeoutMs <= 0) return { ...metric, error: 'there was no time left in the status budget for this telemetry probe' };
    const result = await runner(probe.command, { timeoutMs, maxBuffer: 4096 });
    metric.checkedAt = new Date().toISOString();
    if (!result.ok) return { ...metric, error: result.timedOut ? 'telemetry probe timed out' : describeFailure(result) };
    const text = result.stdout.trim();
    // Reject empty output, booleans, multi-line logs, infinities and units. The
    // unit is configured independently; a missing value never becomes zero.
    if (text.length > 128 || !/^[+-]?(?:\d+(?:\.\d*)?|\.\d+)(?:[eE][+-]?\d+)?$/.test(text) || !Number.isFinite(Number(text))) {
      return { ...metric, error: 'telemetry probe did not return one finite number' };
    }
    return { ...metric, value: Number(text) };
  });
}
