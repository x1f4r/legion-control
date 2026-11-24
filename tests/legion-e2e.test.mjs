import assert from 'node:assert/strict';
import path from 'node:path';
import test from 'node:test';
import { emergencyAbort, realPowerRefusal } from '../scripts/legion-e2e.mjs';

// scripts/legion-e2e.mjs restarts the machine it runs on if calling the restart off fails, so the
// guards around its real power steps are tested here, where nothing is scheduled at all.

test('--real-power is refused unless CI=true says this is a throwaway runner', () => {
  for (const env of [{}, { CI: 'false' }, { CI: '1' }, { CI: 'TRUE' }]) {
    assert.match(realPowerRefusal({ realPower: true }, env), /runs only where CI=true/, JSON.stringify(env));
  }
  assert.equal(realPowerRefusal({ realPower: true }, { CI: 'true' }), null);
  assert.equal(realPowerRefusal({ realPower: false }, {}), null);
});

test('the emergency abort is the operating system\'s own, and on Unix kills only the far node\'s runner', () => {
  assert.deepEqual(emergencyAbort(true, 'C:\\a\\far'), ['shutdown', ['/a']]);
  const farHome = path.join(path.sep, 'home', 'runner', 'work', '_temp', 'legion-e2e+1', 'far');
  const [program, args] = emergencyAbort(false, farHome);
  assert.equal(program, 'pkill');
  assert.deepEqual(args.slice(0, 2), ['-f', '--']);
  const pattern = new RegExp(args[2]);
  const runnerDir = path.join(farHome, 'state', 'power');
  assert.ok(pattern.test(`${path.join(path.sep, 'home', 'runner', '.local', 'bin', 'legion')} power-delay ${runnerDir} 19a2b-1f2e`));
  // Not the driver, whose command line names the far home but never next to the runner's mode.
  assert.ok(!pattern.test(`node scripts/legion-e2e.mjs --far-home ${farHome} --real-power`));
  // Not a runner of another node.
  assert.ok(!pattern.test(`legion power-delay ${path.join(path.sep, 'home', 'me', '.local', 'state', 'legion', 'power')} 1-2`));
});
