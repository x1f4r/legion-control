# Testing

Run checks on the source revision being reviewed. A passing unit suite, a signed artifact and a working window prove different things; retain the command output and artifact hashes for each. CI and final signed builds must be checked again after the last integration change.

## Local checks

Requirements: Node 24 or 26 with `node:sqlite`, the macOS Swift toolchain, JDK 21 plus the Android SDK, and .NET 10. The desktop application uses Avalonia and targets `net10.0`.

From the repository root:

```sh
./scripts/check.sh agent
./scripts/check.sh mac
./scripts/check.sh android
./scripts/check.sh desktop
node --test contract/test/*.test.mjs
```

`./scripts/check.sh` runs all four components on a configured Mac. Set `LEGION_ANDROID_JAVA_HOME` to select JDK 21 and `LEGION_DOTNET` to select a .NET 10 executable. For isolated Mac tests, set `LEGION_CONTROL_HOME` to a temporary directory. Desktop tests create their own temporary state.

The agent check runs, in order: the agent's own check, the syntax of every JavaScript file in the checkout, the invisible-character check, the version check, the agent tests, the release and installer tests in `tests/`, contract validation, the generated-file check and the contract tests, and last the private-data check, so a private value it finds never hides what the tests would have said. The Mac check runs Swift tests and a release build. Android runs debug/release unit tests and debug lint. Desktop runs its tests and release build.

`npm --prefix agent run check` parses the modules the agent ships and refuses zero-width, bidirectional and byte order mark characters in `agent/src`, `agent/install`, `agent/scripts`, `agent/test` and `agent/package.json`, allowing only a byte order mark at the start of a `.ps1`, which Windows PowerShell 5.1 needs to read a script as UTF-8. `node scripts/check-invisible.mjs` applies the same rule to every file git tracks or would track. A test or comment that means such a character writes it as an escape. Note that some editing tools turn a written `\u` escape into the character itself; the two checks are how that is caught.

### Private data

The repository is public, and `node scripts/check-private-data.mjs` keeps it free of the values that have reached it before. It reads every file git tracks or would track, and each file's name, skipping ignored and binary files, and refuses:

- a home directory whose user is not a placeholder: `/home/<user>`, `/Users/<user>`, `<drive>:\Users\<user>` with `\`, `\\` or `/` between the parts, `/mnt/<drive>/Users/<user>` and `/var/home/<user>`, unless the user is `me`, `runner`, `runneradmin`, `Shared`, `Public` or `Default`;
- a private IPv4 address, or a three-part `192.168` prefix, outside 10.0.0.0/24, 192.168.1.0/24, 192.168.10.0/24 and 192.168.50.0/24 and the few shared values the script lists. The documentation ranges 192.0.2.0/24, 198.51.100.0/24 and 203.0.113.0/24 are not private and pass;
- private terms: the real names, user names, host names and addresses that would identify the maintainers, which a public check cannot spell out.

The terms come from the `LEGION_PRIVATE_TERMS` environment variable, which CI fills from the repository secret of that name, and from `~/.legion-control/private/terms.txt`, outside the repository and never committed. Both have one format: one entry per line, or several separated by commas; a line starting with `#` is a comment; each term is at least three characters and matches case-insensitively in every line and file name. An entry may list the public places its term is allowed in after it, each after a `!`, such as `jdoe ! github.com/jdoe/ ! com.jdoe.`; each place must contain the term and cannot contain a comma. A finding prints the file, the line and the entry number, never the term. Without either source the check says it looked for no private terms and still checks homes and addresses.

Set the secret under **Settings → Secrets and variables → Actions**, or with `gh secret set LEGION_PRIVATE_TERMS < ~/.legion-control/private/terms.txt`. Until it exists, and on pull requests from forks, the CI job checks only homes and addresses.

### Linting the shell and PowerShell scripts

```sh
git ls-files -z -- '*.sh' | xargs -0 shellcheck -S warning
pwsh -NoProfile -File scripts/check-powershell.ps1
```

The PowerShell check needs PSScriptAnalyzer 1.25.0: `Install-Module PSScriptAnalyzer -RequiredVersion 1.25.0 -Scope CurrentUser`. Every PowerShell script in the repository stays ASCII, because Windows PowerShell reads a script without a byte order mark in the ANSI code page. For Android release packaging, also run:

```sh
(cd android && ./gradlew :app:assembleDebug :app:assembleRelease :app:lintRelease --console=plain)
```

The contract validator checks schemas, every indexed fixture, canonical hashes and semantic constraints. The duplicate-site-prefix warning is intentional: matching a private subnet is not proof of location. The client fixture tests use the same contract corpus.

For a focused rerun after an agent change:

```sh
npm --prefix agent run check
npm --prefix agent test
node --test tests/*.test.mjs
node contract/validate.mjs
```

The [Checks workflow](../.github/workflows/checks.yml) runs `scripts/check.sh agent` on Linux, Windows and macOS with Node 24 and 26; the private-data check again with the repository's terms; ShellCheck and PSScriptAnalyzer; Mac on macOS; Android with JDK 21; and desktop on all three operating systems, whose Windows job also publishes the native output. Three release dry runs build what a release would, an unsigned Android release APK then signed the way `android/build-apk.sh` signs it, the desktop archives, and the Mac app without installing it, and read no secret. The Windows agent jobs are the only place the PowerShell dispatcher, the Windows process-tree walk and the Windows installer tests run, so a change to any of them is not checked until those jobs are green. Passing locally does not replace checking CI on the exact PR head.

## Artifacts and native clients

Build the desktop outputs with:

```sh
dotnet publish desktop/LegionControl.Desktop -c Release -r linux-x64 --self-contained true -o dist/linux-x64
dotnet publish desktop/LegionControl.Desktop -c Release -r win-x64 --self-contained true -o dist/win-x64
```

Use `desktop/build.sh all` for the named distribution archives. Signed distribution builds must include the final verified agent bundle; an ordinary developer build may omit it, in which case agent installation must be explicitly unavailable. See the platform READMEs and [security guidance](security.md) for packaging and trust requirements.

On each target OS, give the desktop client an isolated `LEGION_CONTROL_HOME` containing a test setup and private bindings. Then run the published binary:

```sh
legion-control --command version
legion-control --smoke --machine pi --json
legion-control --command doctor --machine pi --deep
legion-control --command history --machine pi --limit 10
legion-control --command bundle --machine pi --out diagnostic-bundle.json
```

These commands exercise the same models and transport as the GUI. A headless pass does not verify window rendering, tray behavior, keyboard navigation, dialogs or notification delivery. Launch the window in a real graphical login session; a Linux `XOpenDisplay` failure from an SSH shell is a display-session failure, not a passed UI check.

Test Mac and desktop state relocation with `LEGION_CONTROL_HOME`: setup, bindings, revisions, preferences, keys and host pins must stay inside that directory. Android owns its private key and bindings inside the app. Its debug-only ADB configuration bridge is documented in the [Android README](../android/README.md); release builds must exclude it.

## Isolated SSH integration

Use a disposable target configuration, separate from production services and controller state. `tests/fixtures/service.mjs` provides file-backed version, latest, busy and action evidence. Controllers must not be rebooted, suspended or have their real workloads updated by the fixture suite.

`tests/integration/ssh-scenarios.mjs` expects an existing test installation:

- `BASE/data/agent/src/index.mjs` and `BASE/data/config.json`;
- `BASE/fixture/service.mjs` and `BASE/fixture/data/`;
- system id `pi`, first service id `demo`;
- an SSH alias with an already verified host key;
- an absolute remote Node path and a base ending in `/legion-control-test`.

Run it only against that prepared target:

```sh
node tests/integration/ssh-scenarios.mjs \
  TEST_SSH_ALIAS /absolute/path/to/node /absolute/path/to/legion-control-test evidence.json
```

The suite mutates the isolated fixture and records raw replies, operation ids, timing and exit codes. It covers manual updates with scheduling off, replay/intent conflicts, busy deferral, queue cancellation and execution, invalid configuration, failed postconditions, policy and peer replication. Preserve the output; do not infer an unrecorded hardware transition from an accepted request.

## Acceptance checks across clients

Exercise each applicable flow on Mac, Android, Linux and Windows. Android has no local-agent/self binding; other peer features apply to it.

| Area | Required observation |
| --- | --- |
| Setup editor and templates | Preview and validation precede saving; unknown JSON fields survive. Private keys, aliases, host identities and current-site choice never enter the published setup. |
| Peer reconciliation | Offline edits from the same base produce divergence. Merge retains both edits and both parents; descendants fast-forward; identical retries are no-ops. A different setup id requires an explicit replacement decision. Keep revision copies and verify fetched hashes. |
| Busy, policy and queues | Missing/failed busy evidence blocks disruptive work. Manual updates ignore the schedule; automatic off remains a scheduler stop. Queue expiry and cancellation come from server records. A running operation or watchdog lock cannot be bypassed by force. |
| Operations and output | Own and other-peer running/queued/recent records appear with phase, expiry and result. Cancelling updates the current footer and history. Configured action output and operation details remain accessible. Lost acknowledgements remain unknown until reconciled by id. |
| Diagnostics and health | Doctor, deep checks, logs and bundle export work; partial status retains explicit unknown values. Configured telemetry preserves zero, identifies failed readings, and collects nothing when absent. Notifications remain opt-in. |
| Fleet profiles | Detection alone installs nothing. Available profiles produce an automatic-off draft; validating and saving uses the current agent-config hash. Unknown provenance/policy blocks the updater. Desktop monitor drafts show installed version and application-managed updates, with no false latest-check failure or automatic-update controls. |
| Trust and restricted access | First contact requires fingerprint approval into an explicitly reserved OS identity. Shared setup edits cannot reserve or replace host keys. Changed/extra keys fail closed. Restricted keys can publish setup but cannot run arbitrary shell or edit agent service configuration. |
| Sites and helpers | An overlapping subnet stays unconfirmed unless privately selected. On-site direct wake falls back to ordered helpers when readiness fails. UDP delivery is not wake proof. Ambiguous command helpers reconcile the same operation before failover; helpers are never woken automatically. |
| Power and self-control | Wake into a requested OS requires authenticated readiness and normal boot confirmation. A system change invalidates the remembered route while retaining backoff. Local self-control spawns configured argv without SSH and warns before boot/sleep. Actual boot identity and target establish a reboot; uptime or disconnection alone does not. |
| Updates and recovery | Golden signatures pass and one-byte tampering fails before upload/execution. Bootstrap over a recognized legacy tree retains recovery material. Signed install/rollback verifies nested self-test and exact outcome. Kill-at-rename tests restore a complete tree; failed client launch restores the previous app. Android verifies before handing the APK to the system installer. |
| Compatibility | A 1.2 client still loads additive setup fields and retains its old behavior. It does not gain remote helper wake or contract-3 peer features. |

For physical wake/boot tests, retain target-side evidence and the authenticated return status. Test both actual sites and both OS routes; fixture targets and UDP loopback listeners do not prove a physical cross-site wake.

## Manual checks before a release

Some behaviour no test here can reach. Run these once on the release candidate and keep what you saw.

- **The Windows restricted dispatcher.** On a Windows machine with the installed forced command (`powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File <base>\bin\dispatch.ps1`), set `SSH_ORIGINAL_COMMAND` to `'<node>' '<base>\bin\launcher.mjs' status` and then to the same with `doctor`, with `NODE_OPTIONS=--import=<file URL of a script that writes a marker file>`. Both replies must be valid JSON for their schemas, `status` must answer `ok: true` (also when the temporary path is an 8.3 short path), `doctor`'s `files.private` and `ssh.shell` must give real verdicts rather than "could not be read", and no marker file may appear. The two PowerShell tests in `agent/test/dispatch.test.mjs` do the same on the Windows CI jobs, and fail rather than skip there.
- **A detached operation outliving its session on Linux.** On a systemd host with linger on and `KillUserProcesses=yes` in `/etc/systemd/logind.conf`, start `legionctl run <a slow action> --detach --op <id>` over ssh, close the session, and confirm from a new one that `legionctl op <id>` finishes with `ran`. `npm --prefix agent test` runs the automated part where a user manager answers.
- **The Mac app's accessibility.** With Accessibility Inspector or VoiceOver, a machine row in the sidebar reads its name and state (for example "pi, asleep or unreachable"), the setup row reads "Setup, revision N", and on a divergence sheet each picker in the form-controls rotor says "Which to keep for" and the entry it decides.
- **The Mac app's update pages.** Look at the Legion Control sidebar page, the first-run page's Legion Control section and the panel's row, with Review, the menu, the Go back and Discard confirmations, and diagnostics export from both the window and the panel. "Collecting diagnostics…" appears only after a save location was chosen, never during or after a cancel.
- **The desktop's wake past a hold.** Let a lone helper's wake command time out, open Wake options for the target, and confirm that only "Run <helper>'s command again" is offered and that its question warns about a second run.

## Recorded integration evidence

Validation for the fleet-control change used a Mac, an Android emulator, a Raspberry Pi with Node 24, and the same Legion laptop booted into Windows and CachyOS. The Pi target used an isolated installation and file-backed test service. Its production gateway and the installed AI tools were not updated.

| Area | Observed result and scope |
| --- | --- |
| Client suites | 206 Swift tests, 191 Android tests per build variant, and 379 desktop tests passed. Android debug/release builds and lint passed; macOS signature checks and Android release-signature verification passed. |
| Agent and tooling | The shared suite passed on macOS and the Pi. CachyOS passed the shared suite and the later worker-validation regressions. Windows checks cover process-tree cancellation, SQLite-worker exit and immediate database deletion, literal npm arguments, install paths with spaces/apostrophes, and packaging through Git Bash GNU tar. The workflow records the full Windows Node 24/26 matrix. |
| SSH workflows | All 11 isolated scenarios passed: bounded status, manual updates with scheduling off, same-id replay, intent conflicts, busy deferral, queue cancellation/execution, invalid configuration, failed postconditions, wake packets, divergent setup edits and diagnostics. |
| Agent installation | The isolated Pi accepted a signed bootstrap over 2.1.0, retained recovery material, and completed signed install and rollback. Android installed the bundled 3.0.0 agent through its restricted key and displayed the confirmed installed outcome. Broken worker files and tampered bundles fail before promotion. |
| Installers and scheduling | Install/reinstall checks preserved configuration in test paths with spaces and apostrophes. A uniquely named Windows scheduled task ran the stable launcher against an empty, automatic-off configuration and finished with result zero; the task was removed afterward. |
| Native Windows and Linux | Both distribution builds authenticated to the Pi. Actions and service-update round trips succeeded. The windows rendered in their actual graphical sessions. Linux tray registration was observed on D-Bus and repeated exits were clean. |
| Mac and Android UI | The Mac completed a configured action and validated/saved an administrative service preview. Android displayed the current agent version, updated the fixture, queued a restart while busy, and cancelled it with matching server history and footer. |
| AI-tool profiles | Read-only discovery on the Mac identified native Claude Code and npm Codex CLI/OpenCode. An isolated automatic-off configuration reported their actual versions and available updates. Claude desktop, Codex/ChatGPT desktop and Antigravity reported installed versions with `canUpdate=false`. No real product updater was run. |
| Secret scan | Source scanning found no credentials after excluding the embedded public verification key and a synthetic fixture fingerprint. Export tests cover short configured secrets echoed through diagnostics, without altering ordinary action/log output. |

Check the [PR checks](https://github.com/x1f4r/legion-control/pull/3/checks) for the exact reviewed revision. Locally recorded screenshots, raw replies and artifact hashes distinguish runtime evidence from unit coverage; a later source or bundle change requires the affected checks to run again.

Physical wake between the future sites has not been exercised. Vendor desktop applications without a verified unattended updater remain monitor-only; monitoring does not enable their vendor updater. Grok requires an identified installation before an updater can be provisioned. Native Claude policy and version discovery were verified read-only, so no successful real-tool update is claimed. Android runtime UI checks used an emulator, not a physical phone.
