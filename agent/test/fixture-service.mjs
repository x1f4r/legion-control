// A tiny controllable service, so the agent can be exercised end to end without
// installing anything. Every verb reads its behaviour out of a file in the data
// directory, which is how a test makes it busy, makes its updater a no-op, or
// makes it refuse to stop.
import fs from 'node:fs';
import path from 'node:path';

const [verb, dir] = process.argv.slice(2);
const file = (name) => path.join(dir, name);
const read = (name, fallback = '') => {
  try {
    return fs.readFileSync(file(name), 'utf8').trim();
  } catch {
    return fallback;
  }
};
const write = (name, value) => fs.writeFileSync(file(name), String(value));
const bump = (name) => write(name, Number(read(name, '0')) + 1);

switch (verb) {
  case 'installed':
    if (read('installed-unreadable') === 'on') process.exit(1);
    process.stdout.write(read('installed', '1.0.0'));
    break;
  case 'latest':
    if (read('latest-fails') === 'on') process.exit(1);
    process.stdout.write(read('latest', '1.0.0'));
    break;
  case 'update':
    bump('update-runs');
    if (read('update-noop') === 'on') break;
    if (read('update-fail') === 'on') process.exit(3);
    write('installed', read('latest', '1.0.0'));
    break;
  case 'verify':
    process.exit(read('verify-fails') === 'on' ? 1 : 0);
  case 'rollback':
    bump('rollback-runs');
    write('installed', read('rollback-to', '1.0.0'));
    break;
  case 'running':
    process.exit(read('running', 'on') === 'on' ? 0 : 1);
  case 'start':
    bump('start-runs');
    write('running', 'on');
    break;
  case 'stop':
    bump('stop-runs');
    if (read('stop-refuses') === 'on') process.exit(1);
    write('running', 'off');
    break;
  case 'health':
    process.exit(read('running', 'on') === 'on' ? 0 : 1);
  case 'busy':
    // The drain is what makes a "busy during preparation" test possible: it can
    // flip the machine busy at exactly the moment the update is about to stop it.
    process.exit(read('busy') === 'on' ? 1 : 0);
  case 'drain':
    bump('drain-runs');
    if (read('drain-makes-busy') === 'on') write('busy', 'on');
    process.exit(read('drain-fails') === 'on' ? 1 : 0);
  case 'action':
    bump('action-runs');
    process.stdout.write('action output line');
    break;
  case 'slow': {
    const until = Date.now() + Number(read('slow-seconds', '0')) * 1000;
    while (Date.now() < until) {
      /* deliberately blocking: this is what a slow probe looks like */
    }
    break;
  }
  default:
    process.exit(9);
}
