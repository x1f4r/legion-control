# Operations

What to reach for when something is not behaving: how boot switching actually works, what to
do when a machine will not wake, where the logs and the state live, how to get out of a failed
update, and how to inspect or remove the scheduled jobs. The README covers what the project is
and how to install it; `configuration.md` covers every key of both config files.

Paths below use `<base>` for the agent's install base: `~/.legion-control` on Linux and macOS,
`C:\Users\<user>\.legion-control` on Windows.

## Boot switching

A dual boot machine has two boot paths and they are usually not symmetrical. The typical
arrangement, and the one the reference config describes: the firmware's BootOrder has the
Linux boot loader first, so with nothing else arranged the machine boots Linux, and Windows is
reached by overriding that for one boot.

### Linux to Windows

With a target of method `efi-bootnext`, the agent runs `efibootmgr`, finds the entry whose
description matches the configured regex (`^Windows Boot Manager\b` in the example), and arms
it:

    sudo efibootmgr --bootnext 0005

Then it schedules a reboot three seconds out, so the JSON reply can still be printed and the
ssh session can close cleanly before the machine goes down.

The `Boot####` number is resolved fresh every time and never cached: some firmware (Insyde on
Lenovo machines, for one) garbage-collects and renumbers NVRAM entries, so a number that was
right last week can point somewhere else today. Anchor the regex so it cannot match a recovery
entry or an installer stick: a BootNext into either of those comes up in a menu with no network
and no ssh, which means a trip to the machine. After setting BootNext the agent reads the
variables back and refuses to reboot if the value did not stick.

### Windows to Linux

With a target of method `clear-bootsequence`:

    bcdedit /deletevalue "{fwbootmgr}" bootsequence
    shutdown /r /t 3

Clearing `bootsequence` makes the next boot fall through BootOrder to whatever is first there.
Nothing touches the Linux loader's own entry; the mechanism is "stop overriding and let the
normal order happen". The `bootsequence` method sets it to a named entry instead, for machines
where the Linux loader is not first.

`bcdedit` needs an elevated token. An ssh login as a member of Administrators has one, because
Windows OpenSSH does not apply UAC filtering to that group; a plain desktop PowerShell does
not. If the agent reports that the arming was refused, run the two lines above from an elevated
PowerShell on the machine itself.

### A startup task that re-arms Windows

Some setups run a task at Windows startup that puts `bootsequence` back to `{bootmgr}`, so an
ordinary reboot from a live Windows session returns to Windows instead of falling through to
Linux. It looks like an obstacle and is a safety net: the task only runs if Windows booted
successfully, so a Windows that cannot boot leaves `bootsequence` clear and the machine falls
through to Linux, where it is still reachable over ssh. Windows wins while Windows works,
Linux catches everything else. If you have such a task, leave it alone and keep the Linux
loader first in BootOrder; the agent's `clear-bootsequence` works with it, not against it.

### Encrypted roots

A remote reboot into Linux only works unattended if the root filesystem unlocks without a
passphrase prompt: a TPM-bound keyslot, or no encryption. Otherwise the machine sits at the
prompt until someone types at it.

## Putting a machine to sleep

`legionctl sleep` suspends the machine it runs on:

    sudo -n systemctl suspend            # Linux, default
    <psshutdown> -d -t 3 -accepteula     # Windows, the first of sleep.tools that exists
    pmset sleepnow                       # macOS

Windows is the interesting one. On some machines `rundll32 powrprof.dll,SetSuspendState` and
the .NET `Application.SetSuspendState` are silently vetoed by the power policy: they exit 0,
print nothing, and the box stays awake. Sysinternals `psshutdown` is what actually suspends
them, which is why `sleep.tools` is a list of candidate paths and why the agent refuses, rather
than pretending, when none exists. `-t 3` and not `-t 0`: with a zero delay psshutdown never
returns in a non-interactive session and nothing happens; with a short delay it schedules the
suspend, prints that it has, and exits.

Two things about the answer it gives back:

- `action: "sleeping"` means the command was accepted, not that the machine slept. The suspend
  happens underneath the process that asked for it, so nothing survives to confirm it. If the
  machine is still answering ssh a minute later, the suspend did not take.
- Sleep is busy gated exactly like `boot` and `restart`. A busy machine gets
  `action: "deferred"` and stays awake; `--force` overrides it.

Check with `powercfg /a` (Windows) that a sleep state is actually available, and that Wake on
LAN is armed on the adapter (`ethtool <nic>` on Linux, the adapter's power management tab on
Windows). Wake on LAN is the only reason offering a sleep button is reasonable; if it is not
armed, sleeping a machine remotely is a one-way trip.

### Things that hold sleep off

Three of these cost real time, and each one was invisible until the one before it was fixed.

**Something on the machine takes a sleep hold while ssh is connected.** A monitoring script
that calls `SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED)` whenever a session is
open will block the sleep request that arrives over that very session. Only the process holding
it can drop it, so it has to be stopped first: put the stop command in `sleep.before` and give
it a couple of seconds with `sleep.settle`. Make sure whatever you stop starts itself again on
resume (a Task Scheduler trigger on the Power-Troubleshooter event, for example).

**Sleep is disabled outright.** `systemd` answers `Sleep verb 'suspend' is disabled by config`
when a drop-in under `/etc/systemd/sleep.conf.d/` sets `AllowSuspend=no` or the sleep targets
are masked. A machine that should never suspend *by itself* is better served by a
`systemd-inhibit` idle block, which does not affect an explicit `systemctl suspend`.

**The transport wakes the machine it polls.** If your ssh aliases go through a ProxyCommand
helper that sends a magic packet whenever the machine does not answer, every status poll wakes
it back up and sleep can never stick. Reaching a machine and waking it are different
intentions. The Mac app sets `LEGION_NO_WAKE=1` in the environment of every ssh call it makes,
and a helper can honour that to stay quiet; the Wake button sends its own packet from the app
and never goes through the helper.

Wake on LAN on Linux is armed per boot unless something makes it persistent. A
`systemd.link` file with `WakeOnLan=magic` for the wired interface does that.

## The machine will not wake

Symptoms: the app says the machine is asleep or unreachable, or ssh times out.

**Send the wake packet.** Wake sends the magic packet for the configured MAC three times to
every configured broadcast address on UDP 9 and 7, then polls the probe host on TCP 22 for 45
seconds.

**Check the LAN, not the tunnel.** Wake on LAN is a broadcast on the local segment. From a VPN
or another network the packet goes nowhere, and a tailnet address is useless for waking a
machine that is asleep, because the tunnel daemon is not running while it sleeps. The Android
app greys the button out with a sentence saying so when the phone is not on the machine's
subnet.

**Give it more than 45 seconds.** Cold from powered off, with the firmware POST and the boot
loader's timeout, 45 seconds is optimistic. Try again and wait.

**Awake but the app says the agent is missing.** A different failure, and the app labels it
differently. It means ssh got a shell but the interpreter ran and could not find `index.mjs`.
Re-run the installer on that system. The app tells the two cases apart by what failed: an
interpreter that was not found means the wrong command shape was tried (another system is
running), an interpreter that ran and could not find the script means the right shape and a
genuinely missing agent.

**Windows is up but ssh is refused.** The OpenSSH server has to be running on the Windows
side, and for an administrator account the key has to be in
`C:\ProgramData\ssh\administrators_authorized_keys` with its ACL restricted to SYSTEM and
Administrators; a key in the user's `.ssh` directory does nothing for that account and reads
exactly like the key being wrong.

**The probes get the source penalised.** The apps decide whether a machine is up by opening a
TCP connection to port 22 and closing it without logging in. OpenSSH 10 calls that
"connections without attempting authentication" and penalises the source address:

    srclimit_penalise: <address>/32: activating ipv4 penalty of 15.941 seconds
      for penalty: connections without attempting authentication

The symptom does not look like a firewall: the banner comes back as `Not allowed at this time`
and the connection is reset during key exchange, only from the address that has been probing.
Exempt your own networks in an sshd drop-in:

    PerSourcePenaltyExemptList <your LAN>/24,100.64.0.0/10

The mechanism exists for internet-facing brute force; a tailnet peer is already authenticated
before it can send a packet. If one client starts failing while another is fine, check
`journalctl -u sshd | grep penalise` first.

**The firewall only allows ssh from the LAN.** A tunnel interface with no rule makes "remote
first" silently impossible and everything falls back to the LAN. `ufw allow in on tailscale0
to any port 22 proto tcp`, only that port and only on that interface.

## Reading the agent log

`<base>/legionctl.log`, append-only, rotated at 1 MB into a single `.1` generation. Lines look
like:

    2026-01-01T15:00:00.000Z [update] updated 0.0.32-nightly.20260806.1130 -> 0.0.33-nightly.20260807.1025

The tag in brackets is the CLI verb, so it greps cleanly per command:

    ssh <host> 'tail -50 ~/.legion-control/legionctl.log'
    ssh <host> 'grep "\[update\]" ~/.legion-control/legionctl.log | tail -20'
    ssh <windows-host> 'Get-Content C:\Users\<user>\.legion-control\legionctl.log -Tail 50'

Logging can never fail a command and never writes to stdout, since stdout carries exactly one
JSON object per invocation. So an empty log does not mean nothing ran; it can also mean the
base directory is not writable.

The other places worth looking:

| What | Where |
| --- | --- |
| A service's own output, Linux | `journalctl --user -u <unit> -f` |
| Update cycle output, Linux | `journalctl --user -u legion-control-update.service -n 50` |
| Next scheduled update, Linux | `systemctl --user list-timers legion-control-update.timer` |
| Update task state, Windows | `Get-ScheduledTaskInfo -TaskName "Legion Control Update"` |
| Update cycle output, Mac | `tail -f ~/.legion-control/launchd.log` |
| Update job state, Mac | `launchctl print gui/$(id -u)/com.x1f4r.legion-control.update` |

The Linux update unit writes the agent's JSON object straight into the journal, so the last 50
lines of that unit tell the whole story of the last few cycles.

## The Mac update job

The Mac runs the same agent locally, on a launchd user agent instead of a systemd timer. One
job, one label, every 15 minutes, no run at load:

    com.x1f4r.legion-control.update

It is a user agent in `~/Library/LaunchAgents`, not a daemon. Everything it touches belongs to
your user, and it is bootstrapped into `gui/<uid>` rather than `user/<uid>` because the thing
it eventually acts on is a running GUI app. Nothing here ever wants sudo, and running the
installer under sudo breaks it.

### Inspecting it

    launchctl print gui/$(id -u)/com.x1f4r.legion-control.update
    launchctl list | grep legion-control
    plutil -lint ~/Library/LaunchAgents/com.x1f4r.legion-control.update.plist
    tail -f ~/.legion-control/launchd.log

`launchctl print` is the useful one: `state` and `runs` say whether it has fired, and a
`pid =` line only appears while an instance is alive. The log is the job's stdout and stderr
together, one JSON object per run.

Asking the agent directly, without letting it act:

    node ~/.legion-control/agent/src/index.mjs busy
    node ~/.legion-control/agent/src/index.mjs status

`busy` only reads probes. It can neither quit nor start anything, which is why it is safe to
run at any moment, including from the installer. Do not use `launchctl kickstart` to "test"
the job unless quitting the app right now is genuinely fine: it runs the real update cycle.

### Removing it

    ./mac/install-mac.sh --uninstall-updater

which is exactly these two lines:

    launchctl bootout gui/$(id -u)/com.x1f4r.legion-control.update
    rm ~/Library/LaunchAgents/com.x1f4r.legion-control.update.plist

The uninstall leaves `~/.legion-control` alone on purpose: the app still reads the agent from
there, and the config and state are worth keeping. Both the install and the uninstall refuse to
touch anything while the job is mid-run, since unloading a job kills it and this one may be
holding the maintenance lock or waiting on a bundle swap.

To keep the job installed but stop it acting, turn automatic updates off instead of unloading
it: `node ~/.legion-control/agent/src/index.mjs auto-update off`.

### When something looks wrong here

**"Nothing staged" while a newer version exists.** Normal, and not a fault of this project.
The app downloads its own updates and had not fetched that one yet. `status` says so outright.

**"Update waiting" that never clears.** The build is downloaded but the quit has not happened.
Check `busy` first: while work is in flight, holding the update back is the correct answer. If
nothing is running, read `lastUpdate` in `status` and the `[update]` lines in the log. A
staged build that fails to apply three times is not retried automatically; quit and reopen the
app by hand, or run `update --force`.

**The app did not quit within 60 seconds.** The agent asks with AppleScript and never
escalates, so the app is left running and the update is reported as failed. Usually there is a
dialog waiting for an answer, or a shutdown hook taking its time. Answer it, quit the app by
hand, and the staged build applies on that quit anyway.

**The job is installed but never fires.** A label that was disabled once stays disabled in
launchd's override database, across reboots and across a bootout. Re-running the installer
repairs that, because it runs `launchctl enable` before the bootstrap.

**Node moved.** The plist carries an absolute node path baked in at install time, because a
launchd job does not get the Homebrew prefix on PATH. If node is reinstalled somewhere else,
the job starts failing quietly and the log shows it. Re-run the installer, or point it
somewhere stable with `LEGION_NODE_BIN`.

## Recovering from a failed update

This is the npm cycle. The app kind never installs anything and has its own short list of
failure modes above.

**Read what happened first.** `status` carries `lastUpdate` per service with `at`, `from`,
`to`, `result` and `message`. `result` is one of `ok`, `failed`, `deferred`, `rolled-back`,
`noop`. That plus the `[update]` lines in the log is normally enough to tell whether the
install failed, the health check failed, or the stop failed.

**`rolled-back` means the previous version is back and running.** Nothing to do except find
out why the new build did not come up healthy. It will be retried at the next cycle, so if the
build itself is broken, turn automatic updates off on that system until a newer one lands.

**`failed` with the service down is the case that needs hands.** The most likely cause is a
maintenance lock left behind by an update killed mid-install (a reboot, a power loss, a task
timeout). The lock records its owner's pid and is taken over automatically once that process
is gone, but a lock written by something else on the machine is only judged by age. Fix it by
hand:

    # Linux
    rm -f ~/.legion-control/update.lock
    systemctl --user restart <unit>

    # Windows
    Remove-Item <the lockFile from config.json> -Force
    Start-ScheduledTask -TaskName "<the task from config.json>"

Then `restart --force` and `status` to confirm it is healthy again.

**Pinning a version by hand.** The agent has no pin command. Install the version directly and
turn automatic updates off, otherwise the next cycle drags it forward again:

    npm install --global --prefix <prefix> --no-audit --no-fund <package>@<version>
    node ~/.legion-control/agent/src/index.mjs auto-update off
    node ~/.legion-control/agent/src/index.mjs restart --force

**A pending restart that never clears.** `pendingRestart` means a newer version was wanted
while the machine was busy. It clears on its own at the next idle cycle. If it is stuck, check
`busy`: a crashed session can leave a `pending` row behind, and although the staleness window
is supposed to stop that from blocking forever, a row with no timestamp at all counts as fresh
and does block. `busy` shows every blocking row with its `stale` flag.

**Nothing reads as busy but restarts keep deferring.** Look for `reason: "busy state unknown"`.
That is the fail-closed path: a probe could not be read, so the agent assumes work is in
flight. For the `t3-sqlite` probe it tries a direct read-only open first and a copy of the
database plus its `-wal` and `-shm` siblings second, so seeing this means both failed. Check
the file exists and is readable by the user the agent runs as, especially on Windows when the
update task runs as SYSTEM.

## What lives where

### On every controlled system

| File | Purpose | Survives reinstall |
| --- | --- | --- |
| `<base>/config.json` | everything in `configuration.md` | yes, the installers never write it |
| `<base>/state.json` | `lastUpdate`, pending restarts, the 10 minute version cache, the cached npm prefix | yes |
| `<base>/legionctl.log` | the agent's own log, plus `.1` after rotation | yes |
| `<base>/agent/` | the agent source | no, replaced on every install |
| `<base>/update.lock` | the maintenance lock, unless `lockFile` points elsewhere | transient, removed in a `finally` |

Everything in `state.json` is disposable. Deleting it costs the record of the last update and
one extra registry lookup, nothing else. `config.json` is the only file worth backing up.

### Around the agent

| What | Where |
| --- | --- |
| systemd user units, Linux | `~/.config/systemd/user/legion-control-update.{service,timer}` and any service units |
| Update task, Windows | Task Scheduler, "Legion Control Update" (or the name you passed) |
| Generated task XML, Windows | `<base>\Legion-Control-Update.xml` |
| The launchd job, Mac | `~/Library/LaunchAgents/com.x1f4r.legion-control.update.plist` |
| Update job output, Mac | `<base>/launchd.log` |
| The Mac app | `/Applications/Legion Control.app` |
| The Mac app's config | `~/.config/legion-control/config.json` |
| Last known settings per system, Mac | `UserDefaults` under `com.x1f4r.legion-control` |
| The Android app's config, key and pins | app-private storage; the ssh key is wrapped by the Android keystore |

The remembered settings only exist so the apps can show something for a system that is asleep,
clearly labelled with when it was read. Nothing depends on them being right.

## Quick reference

The three command lines worth remembering, one per platform:

    ssh <host> /usr/bin/node /home/<user>/.legion-control/agent/src/index.mjs status
    ssh <host> node C:\Users\<user>\.legion-control\agent\src\index.mjs status
    node ~/.legion-control/agent/src/index.mjs status

They need no quoting because none of the paths contain spaces, and that is deliberate: the
apps invoke them over ssh unquoted, and the installers refuse a home directory with a space in
it for the same reason. Keep it that way if anything ever moves.

Piping through `jq` makes the output readable, since the agent always prints exactly one object.
