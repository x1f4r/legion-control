# Operations

What to reach for when something is not behaving: how an operation is tracked and recovered, how
to read the queue, what `doctor` tells you, how boot switching actually works, what to do when a
machine will not wake, where the logs and the state live, and how to get out of a failed update.
The README covers what the project is and how to install it; `configuration.md` covers every key
of all three config files; `security.md` covers keys, signing and privileges.

Paths below use `<base>` for the agent's install base: `~/.legion-control` on Linux and macOS,
`C:\Users\<user>\.legion-control` on Windows. `legionctl` stands for the launcher the installers
put in `<base>/bin`, which is not on `PATH`: `<base>/bin/legionctl` on Linux and macOS, and
`powershell -NoProfile -File <base>\bin\legionctl.ps1` on Windows.

## Operations

Everything that changes a machine is an operation: an update, a restart, a boot, a sleep, a
configured action, a whole scheduled cycle, replacing the agent itself. An operation has an id, a kind, a
service or target where that applies, who asked for it, a phase, and a result. It is written down
before the work starts and it outlives the connection that asked for it.

That last part is the point. The old design tied an update to one ssh process and returned its
result only at the end. Cache warming alone can take minutes, an npm install another several, and
every client and every scheduler had a shorter deadline than that sum. A timeout could kill the
work halfway, and nothing on either side could say afterwards what had happened.

### Asking, and finding out later

A long operation is dispatched detached: the agent writes the record, starts the work in a process
that is not a child of the ssh session, and answers immediately that it accepted the request. The
client then polls the record until it finishes. If the link drops in between, nothing is lost — the
work is still running on the machine, and the next poll picks it up where it is.

On Linux the worker runs in a transient scope of the user's systemd manager
(`systemd-run --user --scope`) wherever one answers, so logind's `KillUserProcesses=yes` does not
end it together with the ssh session. That relies on linger, which the installer already asks for.
Without a user manager it starts in a session of its own, as before. On Windows it is a process
created through CIM, outside the session's job.

    legionctl update --service dashboard --detach     # -> accepted, with the operation id
    legionctl op <id> --wait 20                       # long-poll until it finishes
    legionctl history --limit 10                      # what happened recently
    legionctl logs --op <id>                          # that operation's own output

If the detached start fails, the agent runs the work synchronously instead and says so in the
reply. A client has to accept either answer to the same request; both are correct.

### Phases

A record names the phase it is in, which is what makes a stalled operation legible. An update
walks roughly: resolving, recovering, checking busy, warming, locking, draining, stopping,
installing, verifying, starting, health, done — with rolling back instead of done when something
went wrong. A restart, boot, sleep or action has a shorter version of the same walk. The apps show
the phase and the progress note under the service.

### Retrying safely

A retry after an ambiguous handoff — the client could not tell whether its first attempt arrived —
must not start the work twice. Asking again for the same thing reports on the original operation
instead of starting a second reboot or a second install.

This is the one place where being conservative is the whole feature. A disconnection does not
prove a transition happened, even when the last reading was idle and even when the request was
forced. Only an acknowledgement, or state observed afterwards, establishes that a machine actually
rebooted or actually slept. Until then the apps say **outcome not known yet**, in amber, and
resolve it from the operation record when the machine comes back.

### The operation lock

One machine-wide lock is taken by every operation that changes anything, for the whole of it.
Updates used to take a lock and restarts, boots, sleeps and actions did not, which meant a
controller could restart a service while the scheduler was replacing its files, or reboot the
machine mid-install.

A second change while the lock is held is a **conflict**: it is refused, and the refusal names the
operation holding the lock, its kind, its service and its phase, so the app can say what is
already happening rather than "try again". `--force` does not break the lock. Force overrides the
busy gate and nothing else.

The lock itself is an operating system lock on `<base>/op.lock.sqlite`, held by the process doing
the work and released by the kernel the moment that process ends, however it ends. Nothing judges
it stale by age or by a pid written in a file, because a long install is exactly what it exists to
protect. `<base>/op.lock` beside it is a text note of who holds it, for a person reading it over
ssh and for `doctor`; removing it frees nothing, and leaving it behind holds nothing. Never delete
a `.sqlite` lock file: one removed while it is held makes a second lock, which is the failure the
design exists to prevent. Only a per-service lock file an older watchdog wrote is judged by age,
and only when its owner is gone: it is taken over after twenty minutes.

Boot and sleep give the lock back once the transition is dispatched, and keep their record for a
grace of two minutes, in which the machine is expected to go down. The recovery pass below closes
the record afterwards.

### Recovery

Every operation left `running` by a process that is no longer there is reconciled before any new
work starts — and before any network lookup, so a registry that is down cannot stop recovery from
happening. A record is live exactly while the machine lock it took is held, judged together with
its owner's start time and never by a pid alone, so a pid the system has since given to another
process keeps nothing alive. Recovery reads every running record, not only the newest. A scheduler killing a cycle at its time limit, a power cut mid-install, an ssh session
dropping during a reboot: all of them used to leave a record that said an update was in progress
forever. All of them now come back as **interrupted**, naming the phase they reached.

For an update interrupted between stopping and starting, recovery puts the service back up, and
where the installed version is neither the one it started from nor the one it was going to,
attempts the rollback. A cycle never ends with the service left stopped. When more than one record
owes the same service its recovery, they are weighed oldest first by when each run started, not
when it was asked for: a queued update taken up after an interrupted one counts as the later of
the two, so the version it was installing is a whole install and is never rolled back.

Boot and sleep are the honest cases. Uptime alone does not prove a machine rebooted when it was
asked to — it proves only that it rebooted. Where the target can be observed, the record says
`rebooted`; where it cannot, the outcome stays unknown rather than being upgraded to success. A
boot record read on a system that is not its target, after the boot identity changed, is
`interrupted` with an unknown outcome rather than failed: the machine went somewhere, and nobody
can say why it is not where it was sent.

### Commands that may still be running

A command a mutation starts is written into `<base>/state/unsettled.json` before it starts and
taken out once it has been seen to end, so what is left in that ledger is a command nobody saw
end: one that ran out of time and could not be seen to stop, or one whose agent died with it, for
example when the ssh session was killed. Every mutation except `self-update` asks the ledger first,
after it takes the machine lock, and is refused while an entry may still be at work. The refusal
names the command and, when it belonged to an operation, that operation.

On Linux and macOS a command that ended by itself releases whatever it left behind. On Windows the
whole process tree a command started is judged, because Windows cannot tell an unfinished helper
from a daemon a start command launched or an app an action opened. So an entry for a command whose
agent died mid-command holds every mutation except `self-update` for as long as anything that
command started is still running. The ways out are to stop that leftover process by hand, or to
restart the machine: a new boot clears the ledger. `self-update` always goes ahead, as the way to
replace an agent that is holding the machine.

A ledger that cannot be read, for example after a bad hand edit, refuses every mutation except
`self-update` until the file is fixed or removed. An operation already running when the file
breaks keeps the ending its own work reached.

`status` reports interrupted records without writing anything, so looking never changes anything.

### Queue until idle

Any disruptive command can be parked instead of refused:

    legionctl restart --service media --when-idle --expires 4h

The agent writes a queued record and answers that it is queued. The next cycle runs it once the
busy gate is clear. There is at most one queued request per thing — a second "restart media when
idle" replaces the first and the reply says which one it replaced.

The expiry is not optional — four hours by default, at least one minute and seven days at the most.
`--expires` takes a duration or a timestamp and is held to that range; one already in the past is
refused with `bad-argument`, and the request waiting in its place is not replaced. A "restart when
idle" still waiting three days later is not a request anybody remembers making, and acting on it
then is worse than dropping it. An expired request is retired with a result that says so, and it
appears in history like any other outcome.

`--when-idle` with `--force` is refused: one asks to wait for the machine and the other not to.
Boot and sleep share one place in the queue, because a machine makes one power transition at a
time, so a newer request of either replaces the older one. A queued request whose service or action
was removed from the config before it ran is finished as failed, with that reason.

    legionctl cancel <id>

cancels a queued request. A *running* operation is not cancellable: there is no safe way to abort
an installer from outside it, and pretending otherwise would be the more dangerous lie. The reply
says so and names the phase it is in.

All four clients show the queue with its expiry and a cancel next to each row.

## The scheduled cycle

Each machine runs one scheduled job — a systemd timer, a Task Scheduler task, a launchd job — that
runs the maintenance cycle every fifteen minutes. It runs whether the apps are open or not.

The cycle takes the operation lock, reconciles interrupted operations, expires and then runs
queued requests in order while the machine is idle, and then walks the services in their
configured order, updating each one that is eligible. One deferred or failed service never stops
the others, and a failed install for one service never rolls back another. Every service gets its
own child record with its own outcome, under one parent record for the cycle. A cycle whose
service has an unknown ending is unknown itself, and says how many services may or may not have
updated. Its recovery also removes temporary `*.tmp-*` files that a write left behind more than ten
minutes ago.

`cycle --force` overrides the busy gate and nothing else: the policy still decides which services
the cycle may touch, and the pause it reads is the one `config.json` holds, which `auto-update pause`
and `policy set` write.

**A cycle fits in its scheduler's time limit.** The shortest limit a scheduler gives a cycle is 30
minutes, systemd's `TimeoutStartSec` and the Windows task's `ExecutionTimeLimit`, and the cycle
keeps one minute of that back, so it plans with 29. Nothing that can leave a service down starts
unless its worst case fits in the time left: an update, checked before npm's cache download and
again before its drain; the restart of a current service that is not answering; a queued request;
and every recovery put-back but the first, which always runs. What does not fit is deferred with
`timed-out`. An update the cycle has no time for gets its own child record and leaves its version
noted as pending; a queued request keeps its place and its expiry; a recovery still owed is said to
be owed. Work whose worst case is longer than the whole budget is never started by a cycle, and has
to be asked for directly. Before an update decides, the only things it does are start a service it
finds down and wait for it, look up the version, and check busy, none of which leaves anything
down. `doctor`'s `scheduler.timeLimit` warns about a scheduler that allows less than 30 minutes;
re-running the installer sets it.

    legionctl cycle --dry-run

reports what the cycle would do right now — which services are eligible, and the reason for each
one that is not — without taking the lock or touching anything. It is the fastest way to answer
"why has this not updated".

This replaced a scheduler that ran a plain `update`, which selected only the first service. On a
machine with more than one service, everything after the first could sit out of date indefinitely
while the app showed automatic updates as on. The installers rewrite the scheduler entry, and
`doctor` flags a scheduler still pointing at the old command.

Fifteen minutes is chosen because the cycle will not touch a busy machine: the retry interval is
the recovery time.

## Doctor

    legionctl doctor
    legionctl doctor --deep
    legionctl doctor --service dashboard

`doctor` answers "is this machine set up correctly" without changing anything. Every check comes
back as ok, a warning or a failure, with a summary, the detail behind it, and a **fix**: the thing
to actually do. It covers, roughly:

| Check | What it answers |
| --- | --- |
| `config.parse`, `config.validate` | does the file parse, and does everything in it make sense |
| `base.writable`, `ops.dir`, `locks` | can the agent write where it needs to, and is a lock stuck |
| `node.version`, `node.sqlite` | is the runtime new enough, and is the SQLite binding available |
| `npm.available`, `npm.prefix` | is npm there, and where does a global install land |
| `service.<id>.process` | is the process adapter right, and is the service running |
| `service.<id>.health` | is it answering |
| `service.<id>.busy` | can the busy source be read, and is the service monitored at all |
| `service.<id>.relay` | is the configured tunnel process up |
| `service.<id>.latest` | can the newest version be resolved (deep only) |
| `service.<id>.endpoint` | can anything outside the machine reach it (deep only) |
| `scheduler.present`, `scheduler.command`, `scheduler.lastRun` | does the job exist, does its command point at this agent and at the cycle, and when did it last fire |
| `scheduler.timeLimit` | does the scheduler give a cycle the 30 minutes it plans with |
| `files.private` | can only this user read the base: `0700` and `0600` on Linux and macOS, no group access on Windows |
| `agent.signed`, `agent.floor` | does the running agent carry its signed manifest, and which version floor is in effect |
| `ssh.shell` | Windows only: which shell OpenSSH runs commands with, which decides the setup's `shell` |
| `privileges.sudo`, `privileges.boot`, `sleep.tool` | the elevation the configured boot and sleep methods need, and whether the sleep tool exists |
| `controller.copy` | is a controller document stored, and with what identity and revision |
| `session.restricted` | did this session arrive through the dispatcher |
| `clock` | is the machine's clock sane, since every window and expiry depends on it |

`--deep` adds the checks that go out to the network. An ordinary run stays local and fast. The
reply also carries a count of how many checks came back at each level, and how long it took.

    legionctl bundle

collects doctor, status, history, the recent log and the configuration into one object to attach
to a bug report. Sensitive values are redacted — identity file paths and anything token-shaped in
a configured argv array — but read it before you send it. It is your configuration; the redaction
is a safety net, not a guarantee about arbitrary strings you put in a command.

The Mac and desktop clients save the bundle to a file; the phone shares it.

## Recovering from a failed update

**Read what happened first.** `legionctl history` lists recent operations newest first with their
outcome and reason; `legionctl op <id>` gives the whole record including its phases and log; the
`[update]` lines in `<base>/legionctl.log` give the same story in the order it happened. Between
them it is normally obvious whether the install failed, the health check failed or the stop failed.

**A rollback means the previous version is back and running.** Nothing to do except find out why
the new build did not come up healthy. It will be retried at the next cycle, so if the build
itself is broken, pause that service rather than turning the whole machine's schedule off:

    legionctl auto-update pause 2d --service dashboard

**A failure with the service down is the case that needs hands.** The next cycle's recovery puts a
service it finds down back up, so first see whether something is still holding the machine:

    legionctl doctor            # the "locks" check names what holds the lock, if anything does
    legionctl history --limit 5

The machine lock is released when the process holding it ends, so a lock that is held belongs to
something still running. Let it finish, or end that process. Removing `op.lock` frees nothing, and
the `.sqlite` lock files must never be deleted. A refusal that names a command which may still be
running is the ledger above: stop that process, or restart the machine. Then `restart --force` and
`status` to confirm the service is healthy again.

**A postcondition failure means the update command did not do what it said.** The service is
reported as failed rather than updated, with the version actually read, and the configured
rollback command has been run if there is one. That is a real result, not a false alarm: an
updater that exits 0 without installing anything used to be reported as a completed update.

**Pinning a version by hand.** The agent has no pin command. Install the version directly and
pause or disable automatic updates for that service, otherwise the next cycle drags it forward:

    npm install --global --prefix <prefix> --no-audit --no-fund <package>@<version>
    legionctl auto-update off --service <id>
    legionctl restart --force --service <id>

**Nothing reads as busy but everything keeps deferring.** Look for a busy reason of unknown. That
is the fail-closed path: a probe could not be read, so the agent assumes work is in flight. The
per-service busy detail names the evidence it used and the error it hit. The three common causes
are a probe path that is wrong, a probe that timed out, and a service with no busy block at all —
which is unmonitored, and unmonitored is unknown until the config says `{"type": "none"}`.

## The setup will not sync

The shared setup document moves between devices and machines on its own, until it cannot, and then
it asks. `configuration.md` explains the rules; this is what to do when one of them bites.

**"This machine is behind" that never clears.** The client publishes automatically when a machine's
copy is one of its ancestors, so this usually resolves itself on the next poll. If it does not:

    legionctl config meta          # what the machine holds: id, revision, hash, lineage
    legionctl config               # the document itself

Compare the id first. A different setup id is not a sync problem, it is two different setups, and
the app will ask you which one wins rather than picking.

**A push was refused as stale.** The device that pushed is behind: what it tried to send is an
ancestor of what the machine already has. It should fetch and catch up, which it does by itself on
the next poll. Seeing this repeatedly from one device usually means that device is applying an old
document from somewhere — check whether someone restored an old file by hand.

**A push was refused as divergent.** Two devices edited the same setup without seeing each other's
change. Nothing was overwritten; that is the refusal doing its job. Open the divergence view on the
device that got refused, look at the per-entry differences, and merge. The merged document descends
from both branches, so publishing it makes every machine — and the other device — fast-forward onto
it.

Do not reach for the replace override to make this go away. It is there for genuinely different
setups, and using it here is exactly the data loss the lineage exists to prevent.

**Merge and Keep mine are not offered.** Theirs and the shared base have to pass the checks every
device holds a setup to under peer intake. This device's own copy may carry only an unknown site or
a broken wake helper, which taking theirs' side of that entry repairs; any other error in it has to
be fixed in the editor first. Take theirs is always offered. A kept revision this device refuses is
skipped as a base like one that is not kept, so with no usable base every difference is asked about.

**A push is held and not retried.** A machine that refused a revision with `restricted` (this
device's key there is restricted and the revision changes a command) or `config-invalid` is not
sent that revision again until the setup here changes or a person asks; a device with an
unrestricted key publishes it on its next reading. A device whose own setup has an error publishes
nothing and replaces nothing until it is fixed, though it still reads, fetches and merges. A failed
request backs off for a minute per applied hash, and one machine has one setup request in flight at
a time.

**A command from the setup does not run.** A changed command, key, update repository or local agent
waits for approval on each device before it runs there; until then the one approved there runs, or
nothing does. `configuration.md` describes the review.

**Everything conflicts after upgrading.** A machine holding a document written before lineage
existed has no ancestry to check against. The first device that remembers that document's hash can
publish over it normally; a device that has never seen it gets one conflict question and one
decision. It happens once per machine.

**A machine says it cannot carry the setup.** Its agent predates the config command. It is skipped
by sync entirely and never counted as a conflict. Upgrade the agent on it.

**A push was refused while an update was running.** The machine-wide operation lock covers setup
changes too, so a publish does not land in the middle of an install. The client retries after the
operation finishes.

**A push came back as a no-op.** The bytes were already there. That is the correct answer to a
retry after a dropped connection, and clients treat it as success.

## Boot switching

A dual boot machine has two boot paths and they are usually not symmetrical. The typical
arrangement: the firmware's BootOrder has the Linux boot loader first, so with nothing else
arranged the machine boots Linux, and Windows is reached by overriding that for one boot.

### Linux to Windows

With a target of method `efi-bootnext`, the agent runs `efibootmgr`, finds the entry whose
description matches the configured regex, and arms it:

    sudo -n efibootmgr --bootnext 0005

Then it schedules a reboot three seconds out, so the JSON reply can still be printed and the ssh
session can close cleanly before the machine goes down.

The `Boot####` number is resolved fresh every time and never cached: some firmware garbage-collects
and renumbers NVRAM entries, so a number that was right last week can point somewhere else today.
Anchor the regex so it cannot match a recovery entry or an installer stick: a BootNext into either
of those comes up in a menu with no network and no ssh, which means a trip to the machine. After
setting BootNext the agent reads the variables back and refuses to reboot if the value did not
stick.

### Windows to Linux

With a target of method `clear-bootsequence`:

    bcdedit /deletevalue "{fwbootmgr}" bootsequence
    shutdown /r /t 3

Clearing `bootsequence` makes the next boot fall through BootOrder to whatever is first there.
Nothing touches the Linux loader's own entry; the mechanism is "stop overriding and let the normal
order happen". The `bootsequence` method sets it to a named entry instead, for machines where the
Linux loader is not first.

`bcdedit` needs an elevated token. An ssh login as a member of Administrators has one, because
Windows OpenSSH does not apply UAC filtering to that group; a plain desktop PowerShell does not. If
the agent reports that the arming was refused, run the two lines above from an elevated PowerShell
on the machine itself.

### A startup task that re-arms Windows

Some setups run a task at Windows startup that puts `bootsequence` back, so an ordinary reboot from
a live Windows session returns to Windows instead of falling through to Linux. It looks like an
obstacle and is a safety net: the task only runs if Windows booted successfully, so a Windows that
cannot boot leaves `bootsequence` clear and the machine falls through to Linux, where it is still
reachable over ssh. Windows wins while Windows works, Linux catches everything else. If you have
such a task, leave it alone and keep the Linux loader first in BootOrder.

### Encrypted roots

A remote reboot into Linux only works unattended if the root filesystem unlocks without a
passphrase prompt: a TPM-bound keyslot, or no encryption. Otherwise the machine sits at the prompt
until someone types at it.

## Putting a machine to sleep

    sudo -n systemctl suspend            # Linux, default
    <psshutdown> -d -t 3 -accepteula     # Windows, the first of sleep.tools that exists
    pmset sleepnow                       # macOS

Windows is the interesting one. On some machines `rundll32 powrprof.dll,SetSuspendState` and the
.NET `Application.SetSuspendState` are silently vetoed by the power policy: they exit 0, print
nothing, and the box stays awake. Sysinternals `psshutdown` is what actually suspends them, which
is why `sleep.tools` is a list of candidate paths and why the agent refuses, rather than
pretending, when none exists. `-t 3` and not `-t 0`: with a zero delay psshutdown never returns in
a non-interactive session and nothing happens; with a short delay it schedules the suspend, prints
that it has, and exits.

Two things about the answer it gives back:

- A `sleeping` result means the command was accepted, not that the machine slept. The suspend
  happens underneath the process that asked for it, so nothing survives to confirm it. If the
  machine is still answering ssh a minute later, the suspend did not take, and the operation record
  will say the outcome is unknown rather than claiming it slept.
- Sleep is busy gated exactly like boot and restart. A busy machine defers and stays awake;
  `--force` overrides that and nothing else.

Check with `powercfg /a` (Windows) that a sleep state is actually available, and that Wake on LAN
is armed on the adapter (`ethtool <nic>` on Linux, the adapter's power management tab on Windows).
Wake on LAN is the only reason offering a sleep button is reasonable; if it is not armed, sleeping
a machine remotely is a one-way trip.

### Things that hold sleep off

**Something on the machine takes a sleep hold while ssh is connected.** A monitoring script that
calls `SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED)` whenever a session is open will
block the sleep request that arrives over that very session. Only the process holding it can drop
it, so it has to be stopped first: put the stop command in `sleep.before` and give it a couple of
seconds with `sleep.settle`. Make sure whatever you stop starts itself again on resume.

**Sleep is disabled outright.** `systemd` answers `Sleep verb 'suspend' is disabled by config` when
a drop-in under `/etc/systemd/sleep.conf.d/` sets `AllowSuspend=no` or the sleep targets are
masked. A machine that should never suspend *by itself* is better served by a `systemd-inhibit`
idle block, which does not affect an explicit `systemctl suspend`.

**The transport wakes the machine it polls.** If your ssh aliases go through a ProxyCommand helper
that sends a magic packet whenever the machine does not answer, every status poll wakes it back up
and sleep can never stick. Reaching a machine and waking it are different intentions. The clients
set `LEGION_NO_WAKE=1` in the environment of every ssh call they make, and a helper can honour that
to stay quiet; the Wake button sends its own packet and never goes through the helper.

Wake on LAN on Linux is armed per boot unless something makes it persistent. A `systemd.link` file
with `WakeOnLan=magic` for the wired interface does that.

## The machine will not wake

**Find out which route it took.** A wake is either a packet this device sent, or an action it asked
another machine to run. The apps say which, and the failure message names each helper it tried and
why that one did not work. Start there rather than guessing.

**Check the LAN, not the tunnel.** Wake on LAN is a broadcast on the local segment. From a VPN or
another network the packet goes nowhere, and a tunnel address is useless for waking a machine that
is asleep, because the tunnel daemon is not running while it sleeps. That is what wake helpers are
for: a machine already awake on the target's network sends the packet instead.

**"Site unconfirmed" means it could not tell where you are.** Two sites with the same private
subnet — which is most households with two routers of the same make — look identical from the
inside. Rather than guess, the client behaves as if you were off-site and goes through the helpers,
which works either way. Set `currentSite` in your bindings to say where you actually are; see
`configuration.md`.

**A packet sent is not a machine woken.** UDP has no acknowledgement. The agent can only report
that it put datagrams on the wire. What decides success is an authenticated reply from the target,
polled every few seconds; if none comes, the client moves on to the configured helpers rather than
claiming it worked.

**Give it more than forty-five seconds.** Cold from powered off, with the firmware POST and the
boot loader's timeout, forty-five seconds is optimistic. Try again and wait.

**No helper is available.** The message lists each one and its reason: unreachable, action failed,
or itself asleep. Nothing wakes a helper automatically — that would turn the machine you are trying
not to leave running into a machine that is always running. If a helper can itself be woken, the
apps offer that as a separate thing to choose. If every helper for a site is a machine that sleeps,
the editor says so when you save the setup, not when you need it at midnight.

**Check the helper's own history.** A wake action is an ordinary operation on the helper, so
`legionctl history --kind run` there shows whether it ran and what it printed — how many packets
went to which addresses. That is the difference between "the helper never got the request" and "the
helper sent it and the target ignored it".

**Awake but the app says the agent is missing.** A different failure, and the apps label it
differently. It means ssh got a shell but the interpreter ran and could not find `index.mjs`.
Re-run the installer on that system, or use the app's own install action. The apps tell the two
cases apart by what failed: an interpreter that was not found means the wrong command shape was
tried, so another system is running; an interpreter that ran and could not find the script means
the right shape and a genuinely missing agent.

**Windows is up but ssh is refused.** The OpenSSH server has to be running on the Windows side, and
for an administrator account the key has to be in
`C:\ProgramData\ssh\administrators_authorized_keys` with its ACL restricted to SYSTEM and
Administrators; a key in the user's `.ssh` directory does nothing for that account and reads
exactly like the key being wrong.

**The host key changed.** This is never fixed silently. A machine that presents a different host
key than the one you pinned is shown as exactly that, with the `ssh-keygen -R` line to run if you
know why it changed — a reinstall, a new disk, a second system on the same address. No client ever
removes a `known_hosts` entry for you, and a host that has been pinned stays pinned. An unknown
host needs an explicit decision to trust it, not a quiet accept-new.

**The probes get the source penalised.** OpenSSH 9.8 and newer penalise a source address for
repeated connections that do not complete authentication:

    srclimit_penalise: <address>/32: activating ipv4 penalty of 15.941 seconds
      for penalty: connections without attempting authentication

The symptom does not look like a firewall: the banner comes back as `Not allowed at this time` and
the connection is reset during key exchange, only from the address that has been probing. The
clients no longer make a bare TCP probe before an ordinary authenticated call on a route they
already know works, and the fallback round that probes several addresses at once is rate-limited
with backoff, so this should not happen from normal use. If it does — from wake polling, or from
another tool on the same address — the server-side exemption is:

    PerSourcePenaltyExemptList <your LAN>/24,100.64.0.0/10

That is a mitigation, not a prerequisite. If one client starts failing while another is fine, check
`journalctl -u sshd | grep penalise` first.

**The firewall only allows ssh from the LAN.** A tunnel interface with no rule makes "remote first"
silently impossible and everything falls back to the LAN. `ufw allow in on tailscale0 to any port
22 proto tcp`, only that port and only on that interface.

**It answers on the wrong address after a reboot into the other system.** Expected, and handled:
after a boot request, or after noticing that a machine is running a different system than it was,
the client forgets the route it had remembered and tries the endpoints hinted for the new system
first, then the LAN one if it is on that site, then the rest. The hint is only a hint — a machine
that answers with a different system id than the endpoint suggested is still believed, and the
route is remembered again from what actually happened. Failing addresses keep their backoff
throughout, so a transition does not turn into a burst of connections.

## Reading the agent log

`<base>/legionctl.log`, append-only, rotated at 1 MB into a single `.1` generation. Lines look
like:

    2026-01-01T15:00:00.000Z [update] updated 1.4.0 -> 1.5.0

The tag in brackets is the CLI verb, so it greps cleanly per command:

    legionctl logs --lines 50
    legionctl logs --op <id>
    ssh <host> 'tail -50 ~/.legion-control/legionctl.log'
    ssh <windows-host> 'Get-Content C:\Users\<user>\.legion-control\legionctl.log -Tail 50'

Logging can never fail a command and never writes to stdout, since stdout carries exactly one JSON
object per invocation. So an empty log does not mean nothing ran; it can also mean the base
directory is not writable, which is what the `base.writable` doctor check is for.

The other places worth looking:

| What | Where |
| --- | --- |
| A service's own output, Linux | `journalctl --user -u <unit> -f` |
| Cycle output, Linux | `journalctl --user -u legion-control-update.service -n 50` |
| Next scheduled cycle, Linux | `systemctl --user list-timers legion-control-update.timer` |
| Cycle state, Windows | `Get-ScheduledTaskInfo -TaskName "Legion Control Update"` |
| Cycle output, Mac | `tail -f ~/.legion-control/launchd.log` |
| Cycle job state, Mac | `launchctl print gui/$(id -u)/com.x1f4r.legion-control.update` |

The Linux unit writes the agent's JSON object straight into the journal, so the last fifty lines of
that unit tell the whole story of the last few cycles.

## The Mac scheduled job

The Mac runs the same agent locally, on a launchd user agent instead of a systemd timer. One job,
one label, every fifteen minutes, no run at load:

    com.x1f4r.legion-control.update

It is a user agent in `~/Library/LaunchAgents`, not a daemon. Everything it touches belongs to your
user, and it is bootstrapped into `gui/<uid>` rather than `user/<uid>` because the thing it
eventually acts on is a running GUI app. Nothing here ever wants sudo, and running the installer
under sudo breaks it.

### Inspecting it

    launchctl print gui/$(id -u)/com.x1f4r.legion-control.update
    launchctl list | grep legion-control
    plutil -lint ~/Library/LaunchAgents/com.x1f4r.legion-control.update.plist
    tail -f ~/.legion-control/launchd.log

`launchctl print` is the useful one: `state` and `runs` say whether it has fired, and a `pid =`
line only appears while an instance is alive.

Asking the agent directly, without letting it act:

    legionctl busy
    legionctl status
    legionctl cycle --dry-run

All three are read-only. Do not use `launchctl kickstart` to "test" the job unless running the real
cycle right now is genuinely fine; `cycle --dry-run` is the test.

### Removing it

    ./mac/install-mac.sh --uninstall-updater

which is exactly these two lines:

    launchctl bootout gui/$(id -u)/com.x1f4r.legion-control.update
    rm ~/Library/LaunchAgents/com.x1f4r.legion-control.update.plist

The uninstall leaves `~/.legion-control` alone on purpose: the app still reads the agent from
there, and the config and state are worth keeping. Both the install and the uninstall refuse to
touch anything while the job is mid-run, since unloading a job kills it and this one may be holding
the operation lock.

To keep the job installed but stop it acting, turn automatic updates off rather than unloading it:
`legionctl auto-update off`. Manual updates keep working.

### When something looks wrong here

**"Nothing staged" while a newer version exists.** Normal, and not a fault of this project. An app
that updates itself downloads its own builds and had not fetched that one yet.

**"Update waiting" that never clears.** The build is downloaded but the quit has not happened.
Check busy first: while work is in flight, holding the update back is the correct answer. If
nothing is running, read the service's last operation and the `[update]` lines in the log. A staged
build that fails to apply three times is not retried automatically.

**The app did not quit within sixty seconds.** The agent asks with AppleScript and never escalates,
so the app is left running and the update is reported as failed. Usually there is a dialog waiting
for an answer. Answer it, quit the app by hand, and the staged build applies on that quit anyway.

**The job is installed but never fires.** A label that was disabled once stays disabled in launchd's
override database, across reboots and across a bootout. Re-running the installer repairs that,
because it runs `launchctl enable` before the bootstrap.

**Node moved.** The plist carries an absolute node path baked in at install time, because a launchd
job does not get the Homebrew prefix on PATH. If node is reinstalled somewhere else, the job starts
failing quietly. Re-run the installer, or point it somewhere stable with `LEGION_NODE_BIN`.

## Removing the agent

    bash ~/legion-control/agent/install/install-linux.sh --uninstall
    powershell -NoProfile -ExecutionPolicy Bypass -File <checkout>\agent\install\install-windows.ps1 -Uninstall

take away the scheduled cycle (the timer and its units, or the task), the launchers in `<base>/bin`
and the agent trees, and keep `config.json`, the stored setup and the operation records, so a
reinstall picks up where it left off. `--uninstall --purge` and `-Uninstall -Purge` remove those as
well. Both refuse to change anything while a cycle or an operation is running, and both refuse a
base that is the home directory or holds it. On the Mac, `./mac/install-mac.sh --uninstall-updater` removes the
job (above) and prints the line that removes the base; when the base also holds the release signing
key, the Android release key or the private terms of the private-data check, it names them so they
can be moved out first.

## What lives where

### On every controlled system

| File | Purpose | Survives reinstall |
| --- | --- | --- |
| `<base>/config.json` | everything in `configuration.md` | yes, the installers never write it |
| `<base>/config.last-good.json` | the last copy that loaded cleanly, kept for comparison | yes |
| `<base>/state/service-<id>.json` | one file per service: its last update, pending version, apply failure counters, runtime pause | yes |
| `<base>/state/cache.json` | the version cache and the probed npm prefix — reconstructible, disposable | yes |
| `<base>/state/.lock` | the lock every state write goes through | transient |
| `<base>/ops/<id>.json` | one file per operation: running, queued and recent history | yes |
| `<base>/controller.json` | the stored setup document, with its identity, revision and lineage beside it | yes |
| `<base>/legionctl.log` | the agent's own log, plus `.1` after rotation | yes |
| `<base>/state/unsettled.json` | commands a mutation started that nobody has seen end yet | until each is seen to end, or the next boot |
| `<base>/agent/` | the agent source | no, replaced on every install |
| `<base>/agent.prev/` | the previous agent tree, kept for rollback after a self-update | until the next one |
| `<base>/agent-floor.json` | the version floor this machine keeps, outside the replaceable tree | yes |
| `<base>/bin/` | the stable launchers (`legionctl` or `legionctl.ps1`, `launcher.mjs`) and the restricted dispatcher | yes, refreshed by every install |
| `<base>/op.lock.sqlite` | the machine-wide operation lock, held by the operating system | never delete it |
| `<base>/op.lock` | a note of who holds that lock, for people and `doctor` | transient |
| `<base>/update.lock` | the per-service maintenance lock, unless `lockFile` points elsewhere | transient |

Reconstructible state is kept separate from state that records what happened, and one operation is
one file, so two operations can never overwrite each other. Every write goes through a uniquely
named temporary file and a rename, serialised by `state/.lock`. The old design used a single
`state.json`, one shared temporary name and an unlocked read-modify-write, and concurrent writers
could lose each other's records — including pending versions and failure counters. It is migrated
into the new layout on the first 3.x run. Deleting `state/cache.json` costs one extra registry
lookup and nothing else. `config.json` is the only file worth backing up.

Operation records are pruned by kind of ending. Only scheduled updates and cycles can repeat one
another: a record repeats an earlier one when it changed nothing (a no-op, or a deferral, a lock or
an operation in progress included; a cycle when every service it walked was one of those) or when
it failed or its outcome is unknown, and its kind, service, action, reason, and from and to versions
are the same. Of each repeated ending only the first and the newest three are kept. Of what remains,
the newest two hundred are kept whatever they say, and a record that failed or whose outcome is
unknown is kept for seven days however many come after it. Nothing is kept past thirty days, and a
record that still owes a service its recovery is never pruned. `history`'s `retained` counts every
record the machine holds.

### Around the agent

| What | Where |
| --- | --- |
| systemd user units, Linux | `~/.config/systemd/user/legion-control-update.{service,timer}` and any service units |
| Scheduled task, Windows | Task Scheduler, "Legion Control Update" (or the name you passed) |
| Generated task XML, Windows | `<base>\Legion-Control-Update.xml` |
| The launchd job, Mac | `~/Library/LaunchAgents/com.x1f4r.legion-control.update.plist` |
| Cycle output, Mac | `<base>/launchd.log` |
| The Mac app | `/Applications/Legion Control.app` |
| Controller config, Mac and Linux | `~/.config/legion-control/config.json` |
| Controller config, Windows | `%APPDATA%\legion-control\config.json` |
| Private bindings | `bindings.json` beside the controller config |
| Kept setup revisions, every device | `revisions/` beside it, filed by hash, last thirty |
| Everything above, under test | one directory named by `LEGION_CONTROL_HOME`, including the generated key and `known_hosts` |
| Last known settings per system, Mac | `UserDefaults` under `com.x1f4r.legion-control` |
| The Android app's config, key and pins | app-private storage; the ssh key is wrapped by the Android keystore |

The remembered settings only exist so the apps can show something for a system that is asleep,
clearly labelled with when it was read. Nothing depends on them being right.

## Quick reference

    ssh <host> '~/.legion-control/bin/legionctl status'
    ssh <host> /usr/bin/node /home/<user>/.legion-control/bin/launcher.mjs status
    ssh <host> "powershell -NoProfile -File C:\Users\<user>\.legion-control\bin\legionctl.ps1 status"
    ~/.legion-control/bin/legionctl status

A key forced through the restricted dispatcher (`<base>/bin/dispatch.sh`, or `dispatch.ps1` on
Windows) runs the same commands through it; `security.md` describes what it allows. Older installs
may still have the agent at `<base>/agent/src/index.mjs`.

Piping through `jq` makes the output readable, since the agent always prints exactly one object.

Paths that need quoting are quoted for the remote shell named in the controller config's
`shell` key — see `configuration.md`. If you are typing commands by hand, quote them yourself for
whichever shell the remote account logs in to.
