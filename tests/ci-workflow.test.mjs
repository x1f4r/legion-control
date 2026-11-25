import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';

// What CI does cannot run here, but what it is told to do can be read. These hold the workflow to
// the rules its comments state, so a change that breaks one fails on the desk and not in the run.
const repository = path.resolve(import.meta.dirname, '..');
// A Windows checkout may give the workflow CRLF line endings; every rule below is read per line.
const read = (file) => fs.readFileSync(path.join(repository, file), 'utf8').replaceAll('\r\n', '\n');
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
  assert.ok(all.size >= 6, [...all.keys()].join(', '));
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

test('the scripts and Rust legs run across Linux, macOS and Windows, and UI browser tests run on Linux', () => {
  const scripts = job('scripts');
  assert.match(scripts, /os: \[ubuntu-latest, macos-latest, windows-latest\]/);
  assert.match(scripts, /bash scripts\/check\.sh scripts/);

  const rust = job('rust');
  assert.match(rust, /'?\["ubuntu-latest", "macos-latest", "windows-latest"\]'?|\[ubuntu-latest, macos-latest, windows-latest\]/);
  assert.match(rust, /bash scripts\/check\.sh rust/);
  assert.match(rust, /node scripts\/legion-tools-doc\.mjs --bin target\/debug\/legion --check/);
  assert.match(rust, /cargo build --lib -p legion-link --features ssh --locked --target aarch64-linux-android/);
  assert.match(rust, /cargo build --lib -p legion-link --features ssh --locked --target aarch64-apple-ios/);

  const ui = job('ui-browser');
  assert.match(ui, /node --test apps\/ui\/test\/browser\.test\.mjs/);
});

test('only private-data reads a repository secret in checks.yml', () => {
  // The release keys never reach CI; the one secret the workflow reads is the private terms list,
  // and only the job that looks for them.
  for (const [id, lines] of all) {
    if (id !== 'private-data') assert.doesNotMatch(lines.join('\n'), /secrets\./, `${id} reads a secret`);
  }
  assert.deepEqual([...checks.matchAll(/secrets\.([A-Z_]+)/g)].map(match => match[1]), ['LEGION_PRIVATE_TERMS']);
  assert.match(job('private-data'), /run: node scripts\/check-private-data\.mjs\n\s+env:\n\s+LEGION_PRIVATE_TERMS: \$\{\{ secrets\.LEGION_PRIVATE_TERMS \}\}/);
});

test('every shell script goes through ShellCheck, and every PowerShell script through a pinned PSScriptAnalyzer', () => {
  assert.match(job('shellcheck'), /git ls-files -z -- '\*\.sh' \| xargs -0 shellcheck -S warning/);
  const powershell = job('powershell');
  const versions = [...powershell.matchAll(/PSScriptAnalyzer -RequiredVersion (\S+)/g)].map(match => match[1]);
  assert.equal(versions.length, 2, 'installed and imported at a version named both times');
  assert.equal(versions[0], versions[1]);
  assert.match(versions[0], /^\d+\.\d+\.\d+$/);
  assert.match(powershell, /\.\/scripts\/check-powershell\.ps1/);
  assert.ok(fs.existsSync(path.join(repository, 'scripts', 'check-powershell.ps1')));
});

test('Dependabot proposes weekly updates for the actions, Cargo and the UI browser test driver', () => {
  const dependabot = read('.github/dependabot.yml');
  const ecosystems = dependabot.split(/\n(?= {2}- package-ecosystem:)/).slice(1);
  const of = (name) => ecosystems.find(block => block.startsWith(`  - package-ecosystem: ${name}\n`)) ?? '';
  assert.match(of('github-actions'), /directory: \/\n/);
  assert.match(of('cargo'), /directory: \/\n/);
  assert.match(of('npm'), /directory: \/apps\/ui\/test\n/);
  for (const block of ecosystems) assert.match(block, /schedule:\n\s+interval: weekly/);
});

test('legion-app.yml covers every platform with timeouts, credential-free checkouts, and no secrets', () => {
  const appWorkflow = read('.github/workflows/legion-app.yml');
  const appJobs = jobs(appWorkflow);
  for (const id of ['rust-check', 'android', 'linux-desktop', 'ios', 'macos-desktop', 'windows-desktop']) {
    assert.ok(appJobs.has(id), `legion-app.yml has no job ${id}`);
  }
  for (const [id, lines] of appJobs) {
    assert.ok(lines.some(line => /^ {4}timeout-minutes: \d+$/.test(line)), `legion-app.yml:${id} has no timeout-minutes`);
    lines.forEach((line, at) => {
      if (/uses: actions\/checkout@/.test(line)) {
        assert.match(lines.slice(at + 1, at + 3).join('\n'), /persist-credentials: false/, `legion-app.yml:${id} keeps the checkout's token`);
      }
    });
    assert.doesNotMatch(lines.join('\n'), /secrets\./, `legion-app.yml:${id} reads a secret`);
  }
  assert.match(appWorkflow, /cancel-in-progress: \$\{\{ github\.event_name == 'pull_request' \|\| \(github\.event_name == 'push' && github\.ref != 'refs\/heads\/main'\) \}\}/);

  // Every platform build job in legion-app.yml and legion-release.yml must stamp LEGION_BUILD_SEQUENCE.
  for (const id of ['android', 'linux-desktop', 'ios', 'macos-desktop', 'windows-desktop']) {
    const body = appJobs.get(id).join('\n');
    assert.match(body, /LEGION_BUILD_SEQUENCE/, `legion-app.yml:${id} does not stamp LEGION_BUILD_SEQUENCE`);
    assert.match(
      body,
      /make-update-set\.mjs check-artifact[\s\S]*--forbid-preview-key/,
      `legion-app.yml:${id} must verify shipped artifacts with check-artifact --forbid-preview-key`,
    );
  }

  // Every packaging build in legion-app.yml must clear its bundle output directory first, and
  // artifact uploads must reference exact paths from build outputs rather than installer globs.
  assert.match(appJobs.get('android').join('\n'), /rm -rf src-tauri\/gen\/android\/app\/build\/outputs\/apk/);
  assert.match(appJobs.get('linux-desktop').join('\n'), /rm -rf src-tauri\/target\/release\/bundle/);
  assert.match(appJobs.get('ios').join('\n'), /rm -rf src-tauri\/gen\/apple\/build/);
  assert.match(appJobs.get('macos-desktop').join('\n'), /rm -rf src-tauri\/target\/release\/bundle/);
  assert.match(
    appJobs.get('windows-desktop').join('\n'),
    /Remove-Item -LiteralPath 'src-tauri\\target\\release\\bundle' -Recurse -Force/,
  );
  assert.doesNotMatch(
    appWorkflow,
    /path:[^\n]*bundle\/(?:deb\/\*\.deb|appimage\/\*\.AppImage|dmg\/\*\.dmg|nsis\/\*\.exe)/,
    'legion-app.yml must upload exact built bundle paths rather than wildcard globs',
  );
  // Desktop preview self-update steps must clean bundle/ on exit so cached target/ never retains
  // preview-keyed installers across runs.
  for (const id of ['linux-desktop', 'macos-desktop']) {
    const body = appJobs.get(id).join('\n');
    const diagStart = body.indexOf('dump_diag()');
    assert.ok(diagStart !== -1, `legion-app.yml:${id} missing dump_diag cleanup trap`);
    assert.match(
      body.slice(diagStart, diagStart + 1400),
      /rm -rf apps\/legion-app\/src-tauri\/target\/release\/bundle/,
      `legion-app.yml:${id} must clean bundle/ inside self-update cleanup trap`,
    );
    assert.match(
      body.slice(diagStart, diagStart + 1400),
      /new-start\.log/,
      `legion-app.yml:${id} must include new-start.log in self-update diagnostics`,
    );
    assert.match(
      body.slice(diagStart, diagStart + 1400),
      /rollback-ps\.txt/,
      `legion-app.yml:${id} must include rollback-ps.txt in self-update diagnostics`,
    );
  }
  {
    const winBody = appJobs.get('windows-desktop').join('\n');
    const finallyStart = winBody.lastIndexOf('} finally {');
    assert.ok(finallyStart !== -1, 'legion-app.yml:windows-desktop missing self-update finally block');
    assert.match(
      winBody.slice(finallyStart),
      /Remove-Item -LiteralPath 'apps\\legion-app\\src-tauri\\target\\release\\bundle' -Recurse -Force/,
      'legion-app.yml:windows-desktop must clean bundle/ in self-update finally block',
    );
    assert.match(
      winBody,
      /new-start\.log/,
      'legion-app.yml:windows-desktop must include new-start.log in self-update diagnostics',
    );
    assert.match(
      winBody,
      /rollback-ps\.txt/,
      'legion-app.yml:windows-desktop must include rollback-ps.txt in self-update diagnostics',
    );
  }

  const releaseWorkflow = read('.github/workflows/legion-release.yml');
  assert.match(releaseWorkflow, /LEGION_BUILD_SEQUENCE:/);
  assert.match(
    releaseWorkflow,
    /make-update-set\.mjs check-artifact[\s\S]*--kind binary[\s\S]*--forbid-preview-key/,
    'legion-release.yml must verify staged release binaries with check-artifact --forbid-preview-key',
  );
  assert.match(
    releaseWorkflow,
    /make-update-set\.mjs check-artifact[\s\S]*--kind deb[\s\S]*--forbid-preview-key/,
    'legion-release.yml must verify staged release .deb packages with check-artifact --forbid-preview-key',
  );
});
