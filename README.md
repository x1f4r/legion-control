# Legion Control

Remote control for the machines on your desk, from a Mac menu bar, a phone, or a Linux or Windows
desktop.

It tells you which system a dual boot machine is running, whether the services you care about on
it are up, healthy and in the middle of something, and lets you wake it, put it to sleep, reboot it
into its other system, update or restart those services, and run any command you have given a name
to, without walking over to it.

It is driven entirely by configuration: any number of machines across any number of places, any
number of systems per machine, any service you can describe as an npm package, a self updating
desktop app, or a few commands. Nothing about your hardware is assumed, and no machine has to be
left running to serve the others.

Every app is a peer. Any of them can edit the setup and hand it to the rest; none of them is in
charge, and a device can be both a thing you control from and a thing you control.

## How it fits together

Four clients, one agent, one contract, and the only thing between them is ssh:

    Mac, phone, Linux or Windows desktop      a machine you control (whichever system is up)
    Legion Control  ──────────ssh──────────▶  node ~/.legion-control/agent/src/index.mjs <command>
       │                                        └─▶ the services on it
       │                                        └─▶ systemd / Task Scheduler / an app bundle
       │                                        └─▶ efibootmgr / bcdedit / grub-reboot
       │
       └──local──▶ node ~/.legion-control/agent/src/index.mjs <command>
                   └─▶ the services on the device the app is running on

There is no daemon, no open port, no credential store and no polling service. Every action is one
`ssh <host> <interpreter> <path to index.mjs> <command>` round trip that prints one JSON object and
exits; every action against the device the app runs on is the same thing without the ssh. Machine
polling stops when its interface closes. App release checks have a separate schedule, described
below. If ssh works, Legion Control works.

Each machine also has a scheduled maintenance cycle — a systemd timer, a Task
Scheduler task, a launchd job — that runs every fifteen minutes whether the apps are open or not.
The agent decides everything about it: whether the schedule is allowed to act, whether there is
anything to install, and above all whether the machine is busy and must be left alone.

The apps do not know which system of a dual boot machine is up, and do not need to. They try the
remembered system's command shape first and fall back to the others. Only the system that is
actually running has both the interpreter and the script at its path, so a wrong guess costs one
extra round trip and never a wrong answer. The agent's own reply is authoritative.

## The agent

Plain ESM JavaScript for Node 24 or newer. No dependencies, no build step. The same source on every
system, with the platform differences behind adapters. Its configuration lives in
`~/.legion-control/config.json` and describes what the agent looks after on *this* system:

- which **system** this is, so the apps and the other system on the machine can name it;
- the **services**: an npm package following a dist-tag, a desktop app that stages its own updates,
  or anything described with version, update and verify commands — each with a process adapter, a
  health probe and a busy probe;
- when each service may be updated **unattended**: a switch, a pause, a maintenance window, and an
  order that respects dependencies between them;
- the **boot targets**: how to arm the firmware so the next boot lands in another system;
- how to **sleep**, including anything that has to be stopped first;
- **actions**: named commands you want a button for.

`docs/configuration.md` is the complete reference. `examples/` holds an agent config for Linux,
Windows and macOS, a minimal one for a machine with nothing to look after, one for a small
always-on machine that exists to wake its neighbours, a fully worked one, the shared setup
document, and a bindings file.

Every command prints exactly one JSON object to stdout and nothing else, including on failure:

| Command | What it does |
| --- | --- |
| `status` | bounded read-only snapshot: system, services, busy, operations, policy, boot targets, actions |
| `busy` | the aggregate busy object plus the evidence behind each service's answer |
| `update`, `restart` | one service, on request, busy-gated |
| `boot <target>`, `sleep` | arm and reboot into another system; suspend |
| `run <action>` | a configured action |
| `cycle` | the scheduled maintenance pass over every eligible service; `--dry-run` explains itself |
| `auto-update`, `policy` | the update policy: automatic, paused until, maintenance windows, per machine or per service |
| `op`, `history`, `logs`, `cancel` | what is running, what ran, what it printed, and calling off a queued request |
| `doctor`, `bundle` | is this machine set up correctly, and one object to attach to a bug report |
| `config` | read or replace the stored controller document |
| `service-config` | administrator-only service and AI tool setup, with validation and a conflict-checked save |
| `self-update` | replace the agent from a signed bundle, keeping the previous tree |
| `version`, `help` | |

Service updates, restarts, configured actions and power operations support `--force` to override
only the busy gate, and `--when-idle` to wait for the machine to become free. Configuration and
agent installation have their own validation and locking rules.

### One setup, four peers

The list of your machines is one document, and every device carries the same copy. Edit it
anywhere — on the phone in the kitchen, on the laptop on a train — and it reaches the rest through
the machines themselves when they reconnect. Offline edits are retained locally. No central
controller has to stay online or approve another device's changes.

Two people editing at once used to be a data-loss bug waiting to happen, because a revision number
cannot tell "newer" from "different". So each version of the document records the hashes of the
versions it grew out of, and a machine accepts a new copy only when it actually descends from the
one it holds. Everything that descends is applied silently in the right direction. Everything that
does not stops and asks, with a per-entry difference and a merge that keeps both sides.

What is true of one device only — which machine it is, its keys, its ssh aliases, which network it
is sitting on — lives in a separate bindings file that is never shared.

### Places, and waking things up

Machines can be in more than one place, and Legion Control knows the difference between "I am on
that network" and "I am not". A `sites` list says where the LANs are; a machine says which one it
is at.

Waking a sleeping machine from the other end of a tunnel is impossible — a magic packet is a
broadcast, and the machine's tunnel daemon is asleep with it. So any machine already awake on that
network can send the packet for you: an ordinary named action, and the agent can send the packet
itself with no extra software installed. List several and they are tried in order.

Two consequences that are deliberate. A helper is never woken automatically to act as a helper,
because that would turn the machine you are trying not to leave running into one that always is.
And a packet sent is never reported as a machine woken: only an authenticated reply from the target
counts.

### Operations, in one paragraph

Everything that changes a machine is written down before it starts, with an id, a phase and a
result, and it outlives the ssh session that asked for it. A long update is dispatched detached and
polled; a dropped link loses nothing; a retry after an ambiguous handoff reports on the original
rather than starting a second install; an operation killed by a power cut comes back as interrupted
naming the phase it reached, with the service put back up. One machine-wide lock means a restart
cannot land in the middle of an install. `docs/operations.md` is the whole story.

## The clients

All four speak the same contract, decode the same fixtures, and offer the same capabilities —
including editing and publishing the shared setup. They differ in how they reach a machine and in
what a platform makes natural.

The shared interface uses the same action names, status meanings and control groups. Active work
stays visible; finished work is under Recent changes. Android uses touch controls, macOS has its
menu bar and native window, and Windows/Linux provide a keyboard-friendly desktop window and tray.
Technical configuration, diagnostics and command output are available in Details views.

Use **Service setup → Add AI tool** to choose a detected installation or a service template.
Profiles preserve the installed tool's update method and expose unsupported or policy-managed
installations explicitly. New profiles start with automatic updates off; enable them in the
service schedule after reviewing the busy protection. Creating service commands requires
administrator SSH access. A restricted control key can operate and schedule an existing service
but cannot create arbitrary commands. See [the profile reference](docs/configuration.md).

### Mac (`mac/`)

SwiftUI inside an AppKit window, SwiftPM, Swift 6 language mode. Ad hoc signed, which is enough for
a locally built app.

It lives in the menu bar. Click the icon, with either button, and everything is in the panel under
it: the state of every machine, Wake / Sleep / Boot into another system, each service's versions,
health, activity and maintenance policy, the operations in flight and the queue, your actions, and
the services on the Mac itself. Confirmations are drawn inside the panel. The window exists for the
detail and for setup, and nothing forces you into it.

Three things about how it behaves:

**Closing the window puts it away, it does not quit.** The menu bar icon is the app. The only way
out is Quit in the panel or Command-Q.

**Machine polling stops when nothing is open.** The poll belongs to whatever is looking, the window
or the panel, and stops when the last of them closes. App release checks continue periodically while
the menu bar app is running.

**The icon is the state.** A symbol per platform for the running system, a sleeping moon when a
machine is unreachable, a warning triangle when it is awake but the agent is missing, a turning
arrow while a reboot is expected.

It uses the system `ssh` binary, so your ssh config, aliases, ProxyCommand and keys all apply.

### Android (`android/`)

The phone version, and a port rather than a second system: the same agent over ssh, the same
commands, the same JSON. It carries its own ssh client and generates its own ed25519 key on first
run, so the phone's access is revoked by deleting one line from `authorized_keys`. It fetches the
setup from any one machine, and edits and publishes it back like any other client — the phone is
not a read-only remote. It dials the machine's endpoints directly, remote address first and LAN as
the fallback. `android/README.md` covers building and installing it.

### Linux and Windows desktop (`desktop/`)

Avalonia 11 on .NET 10, one project, a native window and a tray icon. Linux x64, Linux ARM64 and
Windows x64 are cross-published as self-contained directories, so no .NET installation is needed
on the target and no Windows toolchain is needed to build the Windows binary. Keep all extracted
files together.

It shells out to the system `ssh` exactly as the Mac client does, which means it reuses your ssh
config, aliases and keys; Windows 10 and later ship `ssh.exe`, and Linux has openssh-client. Direct
endpoints from the controller config are dialled in order, remote first. It generates
`~/.ssh/legion-control_ed25519` on first run and shows the public key to authorise.

It can edit and publish the shared setup, or follow one published elsewhere, exactly as the other
clients do. No device has a privileged position.

It also has a headless smoke mode that drives the same client models and the same transport as the
window, which is what lets the published binaries be used over ssh on a real Linux or Windows
host without a display. The Linux ARM64 archive also runs the headless controller on a Raspberry Pi
with a 64-bit operating system. See [desktop usage](desktop/README.md).

### App update suggestions

The apps check for their own releases at startup and on return to the foreground, normally no more
than once every fifteen minutes. An available update stays visible without opening a dialog on its
own. Periodic checks depend on the platform:

| Client | Periodic checks | After a failed check |
| --- | --- | --- |
| Mac | every six hours while the app runs, including with its window and panel closed | retry after ten minutes |
| Android | every six hours while visible; resume checks on foreground return | retry after fifteen minutes while visible |
| Linux / Windows | every six hours while the window is active; check on activation | next eligible activation or periodic check |

The desktop's local `checkForAppUpdates` preference defaults to `true`. Manual checks bypass the
foreground throttle. A failed check keeps a previously discovered update visible; changing the release
repository clears that offer and invalidates an open installation review.

Installation always starts with an explicit action. On Mac, **Install** downloads, verifies, replaces
and restarts the app. On Android, **Review** leads to separate **Download** and **Install** actions,
then Android's package installer. On Linux and Windows, **Review update** offers **Download and
verify**, followed by **Install and restart**. Every client verifies the signed manifest and its own
platform's artifact before installation. These app suggestions are separate from the agent's
scheduled service updates.

## Installing

The agent has to be on a system before the apps can drive it. Order does not otherwise matter.

### A Linux system

Copy the repository across, then run the installer as your own user, not under sudo. It installs
systemd user units and writes into `$HOME`, and a root run would leave root-owned files the agent
cannot write.

    scp -r legion-control <host>:~/
    ssh <host> 'bash ~/legion-control/agent/install/install-linux.sh'

It checks Node, copies the agent to `~/.legion-control/agent`, renders and enables
`legion-control-update.service` and `legion-control-update.timer` pointing at the maintenance
cycle, and prints the state. It is safe to re-run, and it rewrites an older scheduler entry that
still points at the single-service command.

It does not write `config.json`, on purpose: an installer that stamped a fresh config over the top
would silently flip settings back every time it ran. Copy one from `examples/` and edit it.

Enable linger for the account so the units survive logout and come up on boot with nobody signed
in: `sudo loginctl enable-linger <user>`. Booting into another system and sleeping need narrow
non-interactive sudo rules — `docs/security.md` lists exactly which, and how to grant one helper
instead of several.

### A Windows system

    scp -r legion-control <host>:C:/Users/<user>/
    ssh <host> "powershell -NoProfile -ExecutionPolicy Bypass -File C:\Users\<user>\legion-control\agent\install\install-windows.ps1"

It copies the agent to `C:\Users\<user>\.legion-control\agent` and creates a scheduled task, "Legion
Control Update", that runs the maintenance cycle every fifteen minutes as the current user. If a
task of that name already exists, its action and interval are rewritten and its principal is kept.
Registering a task that runs as SYSTEM needs elevation; the script writes the task XML either way
and, when the session is not elevated, prints the exact one line command to paste into an elevated
PowerShell.

Worth knowing: an ssh login as a member of Administrators is elevated on Windows, because OpenSSH
does not apply UAC filtering to it. That is what lets `bcdedit` succeed over ssh, and it also means
such a key has full administrative rights. For that account the authorised keys live in
`C:\ProgramData\ssh\administrators_authorized_keys`, not in the user's `.ssh` directory.

### The Mac

    ./mac/install-mac.sh

Builds the app, installs it into `/Applications`, proves the bundle landed, then deploys the agent
to `~/.legion-control/agent` and installs the launchd job `com.x1f4r.legion-control.update` that
runs the maintenance cycle every fifteen minutes, never at login. `--skip-updater`,
`--updater-only` and `--uninstall-updater` do what they say. Run it as your own user, never under
sudo.

Then write `~/.config/legion-control/config.json`, or let the app's setup page write the example for
you.

### The phone

    ./android/build-apk.sh

and install the APK. Or take it from a release. On first run it shows the public key to add to
`authorized_keys` on each machine, then **This device → Configuration → Fetch from a machine**
pulls the setup from any one of them.

### The Linux or Windows desktop

Download the matching archive from [the latest release](https://github.com/x1f4r/legion-control/releases/latest):

| System | Archive |
| --- | --- |
| Linux x64 | `Legion-Control-linux-x64.tar.gz` |
| Linux ARM64, including Raspberry Pi with a 64-bit OS | `Legion-Control-linux-arm64.tar.gz` |
| Windows x64 | `Legion-Control-windows-x64.zip` |

Extract the complete archive into an installation directory and run `legion-control` on Linux or
`legion-control.exe` on Windows. To build from a Mac or from the machine itself:

    dotnet publish desktop/LegionControl.Desktop -c Release -r linux-x64 --self-contained true -o dist/linux-x64
    dotnet publish desktop/LegionControl.Desktop -c Release -r linux-arm64 --self-contained true -o dist/linux-arm64
    dotnet publish desktop/LegionControl.Desktop -c Release -r win-x64  --self-contained true -o dist/win-x64

Copy the whole publish directory, including the executable and its adjacent libraries. Its config is
`~/.config/legion-control/config.json` on Linux and `%APPDATA%\legion-control\config.json` on
Windows, with `bindings.json` beside it; on first run it offers the same fetch-from-a-machine
bootstrap as the phone. Linux honors `XDG_CONFIG_HOME` when set. Without a display, use
`./legion-control --smoke` or `./legion-control --command status --machine pi`; these commands run
without initializing the graphical interface.

### Restricting what a key can do

The straightforward setup gives each device's key a shell on the machine. `docs/security.md`
describes the restricted dispatcher, which narrows a key to this agent's own command surface and
nothing else — no shell, no forwarding, no arbitrary command.

## Busy detection, and why everything is gated on it

Every disruptive action goes through one question first: would stopping this lose work? Each
service answers it with its busy probe — a command's exit code, a field in a JSON body, or a
service's own state database.

The rule is fail closed. If a probe cannot be read, busy comes back true with the reason that the
state is unknown. Restarting a service in the middle of real work throws that work away; deferring
only costs a delay, and the next cycle tries again in fifteen minutes. That asymmetry decides every
ambiguous case in the codebase.

Three consequences worth knowing before you write a config:

- **A service with no busy probe is unmonitored, not idle.** It probes as unknown, which fails
  closed, until the config says `{"type": "none"}` out loud. Saying "this has no work worth
  protecting" is a decision, and it has to be made rather than defaulted into.
- **Work in progress never ages out on a timestamp alone.** A long job is exactly what the gate
  exists for. Something running is treated as abandoned only with independent evidence — the
  process is gone, or it started after the work did.
- **Deferring is a question, not a retry.** When the agent holds an action back the apps offer an
  "anyway" button rather than trying again. Interrupting work is always a decision someone made.

## The update cycle

For an npm service, in this order, and the order is the point:

1. Is the scheduler allowed to act on this service right now? A manual request skips this entirely.
2. Resolve the dist-tag from the registry and check it against `versionPattern`. Anything else
   aborts, because a malformed tag would otherwise be handed straight to `npm install`.
3. Already on the target: the only question is whether the service is alive. Healthy is a no-op,
   unhealthy is a restart, honouring busy.
4. Check busy. Busy and not forced: record the wanted version, defer, install nothing.
5. Pre-warm the npm cache so the window with the service stopped stays short.
6. Take the operation lock, then **check busy again** — the answer from before the cache warming is
   minutes stale by now, and this is the check that decides whether anything gets stopped.
7. Drain, if the service has a drain command. Stop the process and poll until it is really gone.
   Install, verify the installed version, start, wait for health.
8. Any failure reinstalls the previous version and reports a rollback. A cycle never ends with the
   service left stopped.

npm replaces files under `node_modules` underneath whatever is running, and a server that imports
modules lazily starts failing on files that changed under its feet. There is no version of that
which is safe to do to a busy server, which is why the cycle refuses rather than being clever.

For a `command` service, the update must prove it happened: the version read afterwards has to
equal the target, or a configured `verify` command has to exit 0. An update command that exits 0
without doing anything is a failure with a reason, not a success — and a configured `rollback`
command runs.

For a desktop app that stages its own updates, the cycle is much shorter: nothing is downloaded,
the agent only notices that a staged build differs from the running one and, when nothing is busy,
asks the app to quit and starts it again, then confirms the version really moved. The quit is asked
for and never forced. A build that fails to apply three times is not retried automatically.

## Boot switching

A dual boot machine is switched by arming the firmware for one boot and rebooting. From Linux,
`efibootmgr --bootnext` with the entry resolved by description on every call, never cached, because
some firmware renumbers NVRAM entries. From Windows, clearing the firmware boot sequence so the next
boot falls through BootOrder, or setting it to a named entry. GRUB users have `grub-reboot`.
Anything else is a `command` target with its own arm and verify.

Arming is always read back and verified before anything reboots. Rebooting an unarmed machine just
returns to the same system, and reporting that as success would send the apps off waiting for one
that never appears. A reboot is scheduled three seconds out so the JSON reply can be printed and the
ssh session can close first.

And a reboot that was asked for is not a reboot that happened. Uptime alone proves only that a
machine rebooted, not that it rebooted because you asked. Where the result cannot be observed, the
apps say the outcome is not known yet rather than claiming success.

`docs/operations.md` has the recovery paths: a machine that will not wake, a failed update, the
operation log, what lives where.

## Trust

Everything the apps and the agent install is signed with one Ed25519 key, whose public half is
committed at `contract/release-public-key.pem` and read by every client and by the agent. A release
manifest names each artifact with its exact basename, size and sha256; the agent tarball carries a
second manifest listing every file inside it. Verification happens before an upload and again on
the machine, and there is no path that skips it. A build with no signed bundle has no install
action rather than a broken one.

`docs/security.md` covers the whole model: per-device keys, host key pinning, the restricted
dispatcher, exactly which sudo rules each configured method needs, redaction in diagnostic bundles,
and what to do if the signing key is lost.

## Checks and releases

    ./scripts/check.sh              # agent, Mac, Android and desktop
    ./scripts/check.sh agent

The same script CI runs. `docs/testing.md` says what each component's checks cover, what a pass
looks like, and which real-hardware scenarios exist — kept separate from the evidence that they
were actually run.

    ./scripts/release.sh 1.3.0            # bump, build, tag, publish
    ./scripts/release.sh 1.3.0 --dry-run  # everything except commit, tag, push and publish

One command, because a release is one thing: every client at one version, one tag, one set of
notes, one signed manifest. It refuses to start on a dirty tree, off the main branch, or on a
version that already has a tag, and it refuses to publish unless the checks pass. Then it bumps
every version in step, builds the five client artifacts and the agent tarball, asks each finished
artifact what version it thinks it is, and signs the manifest over all of them. A tag pointing at a
build that says something else is the one mistake here that cannot be taken back, so the bump is
put back if any of it fails.

The assets are `Legion-Control-macos-arm64.zip`, `Legion-Control-android-arm64.apk`,
`Legion-Control-linux-x64.tar.gz`, `Legion-Control-linux-arm64.tar.gz`,
`Legion-Control-windows-x64.zip`, the agent tarball, and the
signed manifest. Each client selects its own asset by exact name. `dist/` holds them on the way out
and is gitignored.

## Layout

    agent/
      src/
        index.mjs        command dispatch, argument parsing
        contract.mjs     the wire contract: version, reason codes, states, phases, token grammar
        config.mjs       strict config loading, validation, paths, the shared process runner
        state.mjs        locked, atomic per-service state and cache writes
        operations.mjs   operation records, the queue, recovery
        operate.mjs      running one operation through its phases
        lock.mjs         the machine-wide operation lock
        policy.mjs       scheduled eligibility, maintenance windows, service ordering
        scheduler.mjs    the maintenance cycle
        trust.mjs        the release key and signature verification
        providers/       npm, app and command service kinds
        probes/          process, health, busy and relay probes
        service.mjs      start, stop, restart through the process adapter
        boot.mjs         boot targets, reboot, suspend
        update.mjs       the update cycle and the restart
        controller.mjs   the stored controller document, its identity and canonical hash
        log.mjs          the append-only log, rotated at 1 MB
      install/           platform installers, scheduler units, the restricted dispatcher
      test/              node:test unit tests
    contract/            schemas, fixtures, hash vectors, canonical form, the release trust key
    mac/                 the Mac app, its installers and its tests
    android/             the phone app, see android/README.md
    desktop/             the Avalonia client for Linux and Windows, and its tests
    scripts/
      check.sh           the checks, per component
      release.sh         one command from a version number to a published release
      package-agent.mjs  build and sign the agent bundle
      sign-release.mjs   write and sign the release manifest
    tests/               release tooling tests and the throwaway target fixture
    examples/            an agent config per platform, a helper config, the shared setup, bindings
    docs/
      configuration.md   every key of all three config files, and the agent contract
      operations.md      operations, recovery, doctor, logs, where the state lives
      security.md        trust, keys, signing, privileges, the dispatcher
      testing.md         what to run, what a pass looks like, and the evidence

## License

MIT. See `LICENSE`.
