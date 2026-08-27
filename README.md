# Legion Control

Remote control for the machines on your desk, from the menu bar of a Mac or from a phone.

It tells you which system a dual boot machine is running, whether the services you care about
on it are up, healthy and in the middle of something, and lets you wake it, put it to sleep,
reboot it into its other system, update or restart those services, and run any command you
have given a name to, without walking over to it.

It started as a tool for one laptop that dual boots Linux and Windows and runs the T3 Code
server on both, with a Mac that runs T3 Code as well. It is now driven entirely by
configuration: any number of machines, any number of systems per machine, any service you can
describe as an npm package, a self updating desktop app, or a few commands. T3 Code stays in
the repository as the reference example, because it exercises every part of the design.

## How it fits together

Three pieces, one contract, and the only thing between them is ssh:

    Mac or phone                     a machine you control (whichever system is up)
    Legion Control app  ──ssh──▶  node ~/.legion-control/agent/src/index.mjs <command>
       │                            └─▶ the services on it (T3 Code, anything else)
       │                            └─▶ systemd / Task Scheduler / an app bundle
       │                            └─▶ efibootmgr / bcdedit / grub-reboot
       │
       └──local──▶ node ~/.legion-control/agent/src/index.mjs <command>
                   └─▶ the services on the Mac itself

There is no daemon, no open port, no credential store and no polling service. Every action is
exactly one `ssh <host> <interpreter> <path to index.mjs> <command>` round trip that prints one
JSON object and exits; every action against the device the app runs on is the same thing
without the ssh. If the window and the menu bar panel are closed, nothing runs. If ssh works,
Legion Control works.

The one background thing is a scheduled update cycle on each machine (a systemd timer, a Task
Scheduler task, a launchd job) that runs `index.mjs update` every 15 minutes. It runs whether
the apps are open or not, and the agent decides everything about it: whether automatic updates
are on, whether there is anything to install, and above all whether the machine is busy and
must be left alone.

The apps do not know which system of a dual boot machine is up, and do not need to. They try
the remembered system's command shape first and fall back to the others. Only the system that
is actually running has both the interpreter and the script at its path, so a wrong guess costs
one extra round trip and never a wrong answer. The agent's own reply is authoritative.

## The three pieces

### 1. The agent (`agent/`)

Plain ESM JavaScript for Node 22 or newer. No dependencies, no build step. The same source on
every system, with the platform differences behind adapters. Its configuration lives in
`~/.legion-control/config.json` and describes what the agent looks after on *this* system:

- which **system** this is (`{ "id": "linux", "name": "CachyOS" }`), so the apps and the other
  system on the machine can name it;
- the **services**: an npm package followed by a dist-tag, a desktop app that stages its own
  updates, or anything described with `installedVersion`, `latestVersion` and `update` commands,
  each with a process adapter (systemd unit, scheduled task, app bundle, commands), a health
  probe and a busy probe;
- the **boot targets**: how to arm the firmware so the next boot lands in another system;
- how to **sleep**, including anything that has to be stopped first;
- **actions**: named commands you want a button for.

`docs/configuration.md` is the complete reference, with the full JSON contract every command
speaks. `examples/` holds a config for each platform.

Every command prints exactly one JSON object to stdout and nothing else, including on failure:

| Command | What it does |
| --- | --- |
| `status` | read-only snapshot: system, services, busy, boot targets, actions |
| `busy` | just the aggregated busy object |
| `update [--force] [--service <id>]` | one update cycle for one service |
| `restart [--force] [--service <id>]` | restart one service; refuses while busy unless forced |
| `auto-update on\|off` | persists the automatic update setting |
| `boot <target> [--force] [--no-reboot]` | arm the named system and reboot; refuses while busy unless forced |
| `sleep [--force]` | suspend this machine; refuses while busy unless forced |
| `run <action> [--force]` | run a configured action |
| `version`, `help` | |

### 2. The Mac app (`mac/`)

SwiftUI hosted inside an AppKit window, SwiftPM, macOS 26, Swift 6 language mode. Ad hoc signed,
which is enough for a locally built app.

It lives in the menu bar. Click the icon, with either mouse button, and everything is in the
panel under it: the state of every machine, Wake / Sleep / Boot into the other system, each
service's versions, health and activity with Update and Restart, the automatic update switches,
your actions, and the services on the Mac itself. Confirmations are drawn inside the panel.
The window exists for the detail and for setup, and nothing ever forces you into it.

Its configuration is `~/.config/legion-control/config.json`: the machines, how to reach them
over ssh, how to wake them, and which systems each one has. With no config the window opens on
a setup page that writes an example for you. The app reloads the file when it changes.

Three things about how it behaves:

**Closing the window puts it away, it does not quit.** The menu bar icon is the app. The only
way out is Quit in the panel or Command-Q. The Dock icon comes and goes with the window.

**With nothing open it costs nothing.** The 15 second poll belongs to whatever is looking, the
window or the panel, and stops when the last of them closes. There is no timer anywhere else, no
task, no ssh, no process, until something is opened again or an action finishes.

**The icon is the state.** A template symbol per platform for the running system, a sleeping
moon when a machine is unreachable, a warning triangle when it is awake but the agent is
missing, a turning arrow while a reboot is expected.

### 3. The Android app (`android/`)

The phone version of the Mac app. It is a port, not a second system: the same agent over ssh,
the same commands, the same JSON. It carries its own ssh client and generates its own ed25519
key on first run, so the phone's access can be revoked by deleting one line from
`authorized_keys`. It takes the same controller config as the Mac, pasted into its settings,
and dials the machine's endpoints directly, remote address first and LAN as the fallback. Wake
on LAN is offered only when the phone is on the machine's own network, because a magic packet
is a broadcast and a tunnel cannot carry one. `android/README.md` covers building and
installing it.

## Installing

The agent has to be on a system before the apps can drive it. Order does not otherwise matter.

### A Linux system

Copy the repository across, then run the installer as your own user, not under sudo. It
installs systemd user units and writes into `$HOME`, and a root run would leave root-owned
files the agent cannot write.

    scp -r legion-control <host>:~/
    ssh <host> 'bash ~/legion-control/agent/install/install-linux.sh'

It checks Node is at least 22, copies the agent to `~/.legion-control/agent`, renders and
enables `legion-control-update.service` and `legion-control-update.timer`, and prints the
state. With `--with-t3` it also installs `t3@nightly` if T3 Code is not installed at all and
renders the reference `t3-code.service` unit, restarting it only when the unit definition
changed and only when the agent says the machine is idle. It is safe to re-run.

It does not write `config.json`, on purpose: an installer that stamped a fresh config over the
top would silently flip automatic updates back on every time it ran. Copy one from `examples/`
and edit it.

Enable linger for the account so the units survive logout and come up on boot with nobody
signed in: `sudo loginctl enable-linger <user>`. Booting into another system needs
non-interactive sudo for `efibootmgr` and `systemctl reboot`; sleeping needs it for
`systemctl suspend`.

### A Windows system

    scp -r legion-control <host>:C:/Users/<user>/
    ssh <host> "powershell -NoProfile -ExecutionPolicy Bypass -File C:\Users\<user>\legion-control\agent\install\install-windows.ps1"

It copies the agent to `C:\Users\<user>\.legion-control\agent` and creates a scheduled task,
"Legion Control Update", that runs the update cycle every 15 minutes as the current user. If a
task of that name already exists, its action and interval are rewritten and its principal is
kept. Registering a task that runs as SYSTEM needs elevation; the script writes the task XML
either way and, when the session is not elevated, prints the exact one line command to paste
into an elevated PowerShell.

Worth knowing: an ssh login as a member of Administrators is elevated on Windows, because
OpenSSH does not apply UAC filtering to it. That is what lets `bcdedit` succeed over ssh. For
such an account the authorized keys live in
`C:\ProgramData\ssh\administrators_authorized_keys`, not in the user's `.ssh` directory.

### The Mac

    ./mac/install-mac.sh

Builds the app, installs it into `/Applications`, proves the bundle landed, then deploys the
agent to `~/.legion-control/agent` and installs the launchd job
`com.x1f4r.legion-control.update` that runs the update cycle every 15 minutes, never at login.
`--skip-updater`, `--updater-only` and `--uninstall-updater` do what they say. Run it as your
own user, never under sudo; the job is a user agent because the thing it eventually acts on is
an app in your login session.

Then write `~/.config/legion-control/config.json`, or let the app's setup page write the
example for you.

## Busy detection, and why everything is gated on it

Every disruptive action (update, restart, boot, sleep, and any action marked `busyGated`) goes
through one question first: would stopping this lose work? Each service answers it with its
busy probe. For T3 Code the agent reads the server's own state database with `node:sqlite` and
counts running and pending turns and pending approvals inside a staleness window (six hours by
default, because a crashed session leaves a `pending` turn behind forever). A command probe
answers with its exit code. An HTTP probe reads a field out of a JSON body.

The rule is fail closed. If a probe cannot be read, busy comes back `true` with the reason
"busy state unknown". Restarting a service in the middle of real work throws that work away;
deferring only costs a delay, and the next cycle tries again 15 minutes later. That asymmetry
decides every ambiguous case in the codebase: a process probe that fails reports "still
running", an installer that cannot read the answer skips the restart.

When the agent holds an action back it says `action: "deferred"`, and the apps turn that into a
question with an "anyway" button rather than a retry. Interrupting work is always a decision
someone made, never something a stale reading let through.

## The update cycle

For an npm service, in this order, and the order is the point:

1. Automatic updates off and no `--force` means noop.
2. Resolve the dist-tag from the registry and check it against `versionPattern`. Anything else
   aborts, because a malformed tag would otherwise be handed straight to `npm install`.
3. Already on the target: the only question is whether the service is alive. Healthy means
   noop, unhealthy means restart it, honouring busy.
4. Check busy. Busy and not forced: record the wanted version as pending, report `deferred`,
   install nothing.
5. Pre-warm the npm cache so the window with the service stopped stays short.
6. Take the maintenance lock. On a machine where something else already honours a particular
   lock file (a watchdog task, say), point `lockFile` at it.
7. Stop the process and poll until it is really gone, install, verify the installed version,
   start, wait up to two minutes for health.
8. Any failure reinstalls the previous version and reports `rolled-back`. A cycle never ends
   with the service left stopped.

npm replaces files under `node_modules` underneath whatever is running, and a server that
imports modules lazily starts failing on files that changed under its feet. There is no version
of that which is safe to do to a busy server, which is why the cycle refuses rather than being
clever, and why it runs every 15 minutes rather than hourly: since it will not touch a busy
machine, the retry interval is the recovery time.

For a desktop app that stages its own updates, the cycle is much shorter: nothing is
downloaded, the agent only notices that a staged build differs from the running one and, when
nothing is busy, asks the app to quit and starts it again, then confirms the version really
moved. The quit is asked for with AppleScript and never forced: a forced kill loses open work
and can leave the updater half way through swapping the bundle. A build that fails to apply
three times is not retried automatically. An app that was closed is never reopened by the
timer.

## Boot switching

A dual boot machine is switched by arming the firmware for one boot and rebooting. From Linux,
`efibootmgr --bootnext` with the entry resolved by description on every call, never cached,
because some firmware renumbers NVRAM entries. From Windows, clearing `{fwbootmgr}
bootsequence` so the next boot falls through BootOrder, or setting it to a named entry. GRUB
users have `grub-reboot`. Anything else is a `command` target.

Arming is always read back and verified before anything reboots. Rebooting an unarmed machine
just returns to the same system, and reporting that as success would send the apps off waiting
for one that never appears. A reboot is scheduled three seconds out so the JSON reply can be
printed and the ssh session can close before the machine goes down.

`docs/operations.md` has the recovery paths: a machine that will not wake, a failed update, the
logs, what lives where.

## Layout

    agent/
      src/
        index.mjs        command dispatch, argument parsing, the JSON contract
        config.mjs       paths, config and state files, legacy config, the shared process runner
        providers/       npm, app and command service kinds
        probes/          process, health and busy probes
        service.mjs      start, stop, restart through the process adapter
        boot.mjs         boot targets, reboot, suspend
        update.mjs       the update cycle and the restart
        log.mjs          the append-only log, rotated at 1 MB
      install/
        install-linux.sh, install-windows.ps1
        t3-code.service, legion-control-update.service, legion-control-update.timer
      test/              node:test unit tests
    mac/
      Sources/LegionControl/   the app
      install/                 install-mac-agent.sh, the launchd job template
      build.sh, install-mac.sh
    android/                   the phone app, see android/README.md
    examples/                  a config for each side
    docs/
      configuration.md         every key of both config files, and the agent contract
      operations.md            recovery, logs, where the state lives

## License

MIT. See `LICENSE`.
