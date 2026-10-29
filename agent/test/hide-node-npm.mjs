// Loaded with --import ahead of the agent: a Node that has no npm of its own
// beside it, the way some distributions package Node. The agent then takes the
// npm installed in the service's global prefix, which in the tests is the
// stand-in fakeNpm() puts there, and never the real one, which would go to the
// registry and install into the test's home.
import fs from 'node:fs';
import path from 'node:path';

const beside = path.dirname(process.execPath);
const hidden = [path.join(beside, 'node_modules', 'npm'), path.join(beside, '..', 'lib', 'node_modules', 'npm')];
const statSync = fs.statSync;

fs.statSync = function hiddenStatSync(target, ...rest) {
  const resolved = path.resolve(String(target));
  if (hidden.some((directory) => resolved.startsWith(`${directory}${path.sep}`))) {
    throw Object.assign(new Error(`ENOENT: no such file or directory, stat '${resolved}'`), { code: 'ENOENT' });
  }
  return statSync.call(fs, target, ...rest);
};
