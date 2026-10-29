import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';

// What CI does cannot run here, but what it is told to do can be read. These hold the workflow to
// the rules its comments state, so a change that breaks one fails on the desk and not in the run.
const repository = path.resolve(import.meta.dirname, '..');
const read = (file) => fs.readFileSync(path.join(repository, file), 'utf8');
const workflows = fs.readdirSync(path.join(repository, '.github', 'workflows')).filter(name => /\.ya?ml$/.test(name)).map(name => `.github/workflows/${name}`);
const checks = read('.github/workflows/checks.yml');

// The jobs of a workflow by id, each as its lines, read by indentation: a job is a key two spaces
// in under `jobs:`, and every line after it that is indented further, or blank, is the job's.
function jobs(text) {
  const lines = text.split('\n');
  const found = new Map();
  let current = null;
  for (const line of lines.slice(lines.indexOf('jobs:') + 1)) {
    const key = /^ {2}([A-Za-z0-9_-]+):\s*$/.exec(line);
    if (key) found.set(current = key[1], []);
    else if (/^\S/.test(line)) break;
    else if (current && !/^\s*#/.test(line)) found.get(current).push(line);
  }
  return found;
}
const all = jobs(checks);
const job = (id) => {
  assert.ok(all.has(id), `checks.yml has no job ${id}`);
  return all.get(id).join('\n');
};

test('every action is pinned to the full commit of the release named beside it', () => {
  const pins = new Map();
  for (const file of workflows) {
    for (const line of read(file).split('\n').filter(each => /^\s*-?\s*uses:/.test(each))) {
      const pin = /^\s*-?\s*uses: ([\w.-]+\/[\w.-]+(?:\/[\w.-]+)*)@([0-9a-f]{40}) # (v\d+\.\d+\.\d+)$/.exec(line);
      assert.ok(pin, `${file}: ${line.trim()} is not pinned to a commit with its version beside it`);
      // An action of one repository at two commits is a pin that was only half updated.
      const repo = pin[1].split('/').slice(0, 2).join('/');
      const seen = pins.get(repo) ?? `${pin[2]} ${pin[3]}`;
      assert.equal(`${pin[2]} ${pin[3]}`, seen, `${repo} is pinned to two different commits`);
      pins.set(repo, seen);
    }
  }
  assert.ok(pins.size >= 6, [...pins.keys()].join(', '));
});

test('every job has a time limit, and every checkout leaves no credentials behind', () => {
  assert.ok(all.size >= 4, [...all.keys()].join(', '));
  for (const [id, lines] of all) {
    assert.ok(lines.some(line => /^ {4}timeout-minutes: \d+$/.test(line)), `${id} has no timeout-minutes`);
    lines.forEach((line, at) => {
      if (/uses: actions\/checkout@/.test(line)) assert.match(lines.slice(at + 1, at + 3).join('\n'), /persist-credentials: false/, `${id} keeps the checkout's token`);
    });
  }
});

test('a newer push cancels the older run of a pull request, and a push to main is never cancelled', () => {
  const concurrency = /^concurrency:\n((?: {2}.*\n)+)/m.exec(checks)?.[1] ?? '';
  // A run of main in a group of its own, which nothing else joins to cancel it or keep it waiting.
  assert.match(concurrency, /group: checks-\$\{\{ github\.event_name == 'pull_request' && github\.ref \|\| github\.run_id \}\}/);
  assert.match(concurrency, /cancel-in-progress: \$\{\{ github\.event_name == 'pull_request' \}\}/);
});

test('the Gradle wrapper is validated before Gradle runs, and Gradle and NuGet are cached', () => {
  for (const id of ['android']) {
    const lines = job(id);
    const validated = lines.indexOf('gradle/actions/wrapper-validation@');
    assert.ok(validated >= 0, `${id} does not validate the Gradle wrapper`);
    assert.ok(validated < lines.indexOf('actions/setup-java@'), `${id} sets Java up before it validates the wrapper`);
    assert.match(lines, /cache: gradle\n\s+cache-dependency-path: \|\n(?:\s+android\/.+\n){4}/);
  }
  for (const id of ['desktop']) {
    assert.match(job(id), /uses: actions\/cache@[0-9a-f]{40} # v[\d.]+\n\s+with:\n\s+path: ~\/\.nuget\/packages\n\s+key: nuget-\$\{\{ runner\.os \}\}-\$\{\{ hashFiles\('desktop\/\*\*\/\*\.csproj'\) \}\}/);
  }
});

test('Dependabot proposes updates for the actions, the Android build and every desktop project', () => {
  const dependabot = read('.github/dependabot.yml');
  const ecosystems = dependabot.split(/\n(?= {2}- package-ecosystem:)/).slice(1);
  const of = (name) => ecosystems.find(block => block.startsWith(`  - package-ecosystem: ${name}\n`)) ?? '';
  assert.match(of('github-actions'), /directory: \/\n/);
  assert.match(of('gradle'), /directory: \/android\n/);
  const projects = fs.readdirSync(path.join(repository, 'desktop'), { withFileTypes: true })
    .filter(entry => entry.isDirectory() && fs.readdirSync(path.join(repository, 'desktop', entry.name)).some(name => name.endsWith('.csproj')));
  assert.ok(projects.length >= 2);
  for (const project of projects) assert.match(of('nuget'), new RegExp(`- /desktop/${project.name}\\n`), `${project.name} gets no updates`);
  for (const block of ecosystems) assert.match(block, /schedule:\n\s+interval: weekly/);
});
