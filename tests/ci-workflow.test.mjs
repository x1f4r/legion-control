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
  assert.match(
    appJobs.get('linux-desktop').join('\n'),
    /scripts\/build-linux-app-bundles\.sh/,
    'legion-app.yml:linux-desktop must build Linux bundles against the ubuntu:22.04 glibc baseline',
  );
  assert.match(
    appJobs.get('linux-desktop').join('\n'),
    /rustup target add x86_64-unknown-linux-musl/,
    'legion-app.yml:linux-desktop must install the x86_64-unknown-linux-musl target for static Linux preview node builds',
  );
  assert.match(
    appJobs.get('linux-desktop').join('\n'),
    /\bmusl-tools\b/,
    'legion-app.yml:linux-desktop must install musl-tools for static Linux preview node builds',
  );
  assert.match(
    appJobs.get('linux-desktop').join('\n'),
    /scripts\/legion-app-distro-test\.sh/,
    'legion-app.yml:linux-desktop must install-verify the built .deb and AppImage across Debian and Ubuntu containers',
  );
  const previewScript = read('scripts/build-desktop-preview.mjs');
  assert.match(
    previewScript,
    /build-linux-app-bundles\.sh[\s\S]*--preview-key/,
    'scripts/build-desktop-preview.mjs must route Linux desktop app preview builds through scripts/build-linux-app-bundles.sh',
  );
  assert.match(
    previewScript,
    /x86_64-unknown-linux-musl/,
    'scripts/build-desktop-preview.mjs must build the static x86_64-unknown-linux-musl Linux node binary',
  );
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
      body.slice(diagStart, diagStart + 1600),
      /rollback-ps\.txt/,
      `legion-app.yml:${id} must include rollback-ps.txt in self-update diagnostics`,
    );
    assert.match(
      body.slice(diagStart, diagStart + 1600),
      /update\.lock/,
      `legion-app.yml:${id} must include update.lock in self-update diagnostics`,
    );
    assert.match(
      body.slice(diagStart, diagStart + 1600),
      /deferred-\*\.json/,
      `legion-app.yml:${id} must include deferred-*.json in self-update diagnostics`,
    );
    assert.match(
      body,
      /Phase 2: concurrent second start/,
      `legion-app.yml:${id} must verify a concurrent second start during an update`,
    );
    assert.match(
      body,
      /verify_updated\(\)[\s\S]*&& \[ ! -e "\$APP_DATA\/update\.lock" \]/,
      `legion-app.yml:${id} must wait for update.lock release inside verify_updated`,
    );
    assert.match(
      body,
      /verify_updated\(\)[\s\S]*count_app_instances[\s\S]*rollback-\*\.json/,
      `legion-app.yml:${id} must verify single running instance and rollback absence inside verify_updated`,
    );
    assert.match(
      body,
      /FAIL \((?:Linux|macOS) self-update\):/,
      `legion-app.yml:${id} must print a named single-line failure reason before diagnostics`,
    );
  }
  {
    const winBody = appJobs.get('windows-desktop').join('\n');
    assert.match(
      winBody,
      /cargo test --manifest-path apps\/legion-app\/src-tauri\/Cargo\.toml --locked/,
      'legion-app.yml:windows-desktop must run Rust unit tests on Windows',
    );
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
    assert.match(
      winBody,
      /update\.lock/,
      'legion-app.yml:windows-desktop must include update.lock in self-update diagnostics',
    );
    assert.match(
      winBody,
      /deferred-\*\.json/,
      'legion-app.yml:windows-desktop must include deferred-*.json in self-update diagnostics',
    );
    assert.match(
      winBody,
      /Phase 2: concurrent second start/,
      'legion-app.yml:windows-desktop must verify a concurrent second start during an update',
    );
    assert.match(
      winBody,
      /function Verify-Updated[\s\S]*-not \(Test-Path -LiteralPath \$updateLock\)/,
      'legion-app.yml:windows-desktop must wait for update.lock release inside Verify-Updated',
    );
    assert.match(
      winBody,
      /function Verify-Updated[\s\S]*Get-AppInstances[\s\S]*rollback-\*\.json/,
      'legion-app.yml:windows-desktop must verify single running instance and rollback absence inside Verify-Updated',
    );
    assert.match(
      winBody,
      /Write-Host "FAIL \(Windows self-update\):/,
      'legion-app.yml:windows-desktop must print a named single-line failure reason before diagnostics',
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
  assert.doesNotMatch(
    releaseWorkflow,
    /vars\.|LEGION_PREVIEW_UPDATE_PUBKEY\s*:/,
    'legion-release.yml must never read repository variables or pass LEGION_PREVIEW_UPDATE_PUBKEY',
  );
  const unsetMatches = releaseWorkflow.match(/unset LEGION_PREVIEW_UPDATE_KEY LEGION_PREVIEW_UPDATE_PUBKEY/g) ?? [];
  assert.equal(
    unsetMatches.length,
    3,
    'legion-release.yml must unset preview key variables in all three release build steps',
  );
});

test('legion-app.yml preview desktop builds are workflow_dispatch-only, use vars.LEGION_PREVIEW_UPDATE_PUBKEY, and never leak into ordinary builds', () => {
  const appWorkflow = read('.github/workflows/legion-app.yml');
  const appJobs = jobs(appWorkflow);

  // workflow_dispatch defines boolean `preview` (default false) and string `sequence`.
  const dispatchBlock = /^ {2}workflow_dispatch:\n((?: {4}.*\n)+)/m.exec(appWorkflow)?.[1] ?? '';
  assert.match(dispatchBlock, / {6}preview:\n(?: {8}.*\n)*? {8}type: boolean\n {8}default: false/);
  assert.match(dispatchBlock, / {6}sequence:\n(?: {8}.*\n)*? {8}type: string\n {8}default: ''/);

  // Non-desktop jobs never read LEGION_PREVIEW_UPDATE_PUBKEY.
  for (const id of ['rust-check', 'android', 'ios']) {
    assert.doesNotMatch(
      appJobs.get(id).join('\n'),
      /LEGION_PREVIEW_UPDATE_PUBKEY|build-desktop-preview\.mjs/,
      `legion-app.yml:${id} must not build desktop preview artifacts`,
    );
  }

  // Each desktop job gates both its fast-fail validation and its preview build+upload strictly on
  // `github.event_name == 'workflow_dispatch' && inputs.preview`, reads the repository variable
  // (never a secret), and calls the shared build script.
  const desktopExpectations = [
    ['linux-desktop', 'linux', 'legion-linux-preview'],
    ['macos-desktop', 'macos', 'legion-macos-preview'],
    ['windows-desktop', 'windows', 'legion-windows-preview'],
  ];
  for (const [jobId, osName, artifactName] of desktopExpectations) {
    const body = appJobs.get(jobId).join('\n');
    const previewGateRe = /if: \$\{\{ github\.event_name == 'workflow_dispatch' && inputs\.preview \}\}/g;
    const gateCount = (body.match(previewGateRe) ?? []).length;
    assert.equal(
      gateCount,
      3,
      `legion-app.yml:${jobId} must gate validate, build, and upload preview steps on workflow_dispatch && inputs.preview`,
    );
    assert.match(
      body,
      new RegExp(`build-desktop-preview\\.mjs[\\s\\S]*--check-only[\\s\\S]*--os ${osName}`),
      `legion-app.yml:${jobId} must fast-fail validate preview inputs`,
    );
    assert.match(
      body,
      new RegExp(`build-desktop-preview\\.mjs[\\s\\S]*--os ${osName}[\\s\\S]*--out ci-preview/${osName}`),
      `legion-app.yml:${jobId} must run scripts/build-desktop-preview.mjs for ${osName}`,
    );
    assert.match(
      body,
      new RegExp(`name: ${artifactName}\\n\\s+path: ci-preview/${osName}/\\n\\s+if-no-files-found: error`),
      `legion-app.yml:${jobId} must upload ${artifactName}`,
    );
    assert.match(
      body,
      /LEGION_PREVIEW_UPDATE_PUBKEY: \$\{\{ vars\.LEGION_PREVIEW_UPDATE_PUBKEY \}\}/,
      `legion-app.yml:${jobId} must read vars.LEGION_PREVIEW_UPDATE_PUBKEY`,
    );
    // The ordinary `id: build` step must never see LEGION_PREVIEW_UPDATE_PUBKEY or LEGION_PREVIEW_UPDATE_KEY.
    const ordinaryStart = body.indexOf('- id: build');
    const ordinaryEnd = body.indexOf('if-no-files-found: error', ordinaryStart);
    assert.ok(ordinaryStart !== -1 && ordinaryEnd > ordinaryStart, `legion-app.yml:${jobId} missing ordinary build block`);
    assert.doesNotMatch(
      body.slice(ordinaryStart, ordinaryEnd),
      /LEGION_PREVIEW_UPDATE_PUBKEY|LEGION_PREVIEW_UPDATE_KEY/,
      `legion-app.yml:${jobId} ordinary build must never receive preview key variables`,
    );
  }
});
