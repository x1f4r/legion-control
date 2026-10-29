# Legion Control for Linux and Windows

The same app as the Mac one and the phone one, for the two desktops. It reads the same setup
document, talks to the same agent over the same ssh, and follows the same rules about what it will
and will not claim.

Built with Avalonia 11 on .NET 10, published self-contained for Linux x64, Linux ARM64 and Windows x64
so no .NET installation is required. Keep every file from the archive in the installation directory.
OpenSSH is required for remote connections; the window also needs the platform graphics libraries.
The window and the headless mode are the same objects: `--smoke` and `--command` drive
the models the window draws, so a check that passes over ssh is a check of the thing that ships.

## Running it

A bare invocation opens the window. Command flags run without a window and exit.

Choose `Legion-Control-linux-x64.tar.gz`, `Legion-Control-linux-arm64.tar.gz`, or
`Legion-Control-windows-x64.zip` from [the latest release](https://github.com/x1f4r/legion-control/releases/latest).
The ARM64 build supports a Raspberry Pi running a 64-bit Linux OS. Its headless commands do not
initialize Avalonia and need no display session, so the Pi can be a controller over SSH as well as
an agent target. A graphical session is required only for the window and screenshot mode.

```
legion-control                                   the window
legion-control --smoke                           read everything, change nothing, say what is wrong
legion-control --command status  --machine pi
legion-control --command update  --machine pi --service t3 --yes
legion-control --command wake    --machine legion --yes
legion-control --help                            every command
```

Three exit codes, and the third is the reason they exist:

| code | meaning |
| --- | --- |
| 0 | it did what was asked, and that is known |
| 1 | it did not, and that is known |
| 2 | the outcome is not known: a link that went away, a poll that never resolved |

Nothing here returns 0 for an outcome nobody observed. A command whose reply was lost keeps its
operation id, and `--command op --machine ID --op ID` asks the machine what became of it rather than
sending it again.

## Where things live

| what | Linux | Windows |
| --- | --- | --- |
| the setup document | `~/.config/legion-control/config.json` | `%APPDATA%\legion-control\config.json` |
| this device's own settings | `bindings.json` beside it | the same |
| the commands this device approved | `approved-launch.json` beside it | the same |
| earlier setups, by hash | `revisions/` beside it | the same |
| operations this app started | `operations.json` beside it | the same |
| desktop preferences | `desktop-settings.json` beside it | the same |
| ssh's own log while a command runs, deleted after each attempt | `ssh-logs/` beside it | the same |

Linux uses `$XDG_CONFIG_HOME/legion-control/` instead when `XDG_CONFIG_HOME` is set.

Three environment variables move all of it, which is how a test run touches nothing that matters:

- `LEGION_CONTROL_CONFIG` — the setup document to read
- `LEGION_CONTROL_HOME` — everything this app writes. When it is set, the ssh material moves too:
  `$LEGION_CONTROL_HOME/ssh/known_hosts` is passed to ssh explicitly, so a test run cannot pin a
  host key into the real `~/.ssh/known_hosts`. Without it, ssh keeps the user's own configuration
  and this app does not touch their pins.
- `LEGION_CONTROL_SSH` — the ssh binary to run

## The setup, and the other devices

Every device is a peer. This one edits and publishes the shared document exactly as the Mac and the
phone do, and no device is the authority.

What keeps two people's edits from eating each other is ancestry, not revision numbers. Each
revision carries the hashes of the documents it descends from, and a machine accepts a document only
when it descends from the one that machine already holds. So:

- a machine holding an ancestor of this device's document is published to, automatically
- a machine holding a descendant is fetched from, automatically
- anything else stops and asks, with both revisions, who wrote them, and a per-entry diff

`--command setup` shows where each machine stands. `--reconcile-once` does one pass.
`--command setup --machine ID --merge | --keep-mine | --take-theirs | --replace-theirs` are the four
answers, and none of them happens on its own. Merging keeps both sides: entries only one device
changed are taken from that device, and entries both changed are the only ones anybody is asked
about.

`--replace` is the one command that overwrites a different setup, and it is only ever sent after a
person has answered that question in front of a preview of what changes.

## Commands from the shared setup

Ancestry says that a document claims to descend from another, not who wrote it, and any key that may
publish the setup, a restricted one included, can write a descendant. So the values that decide what
this device runs with its own key are never taken from replication alone: each system's agent argv
with the grammar its line is written in, a machine's `ssh.identityFile`, `appUpdates.githubRepo` and
`local`. The document still applies as it arrives. A system whose command changed keeps running the
one this device approved there, and a system with no approved command is not driven at all, until a
person approves the new one here.

`approved-launch.json` holds what this device approved. It is never published. The first start of
this build approves everything in the setup it already applies. Deleting the file makes the next
start do that again, with the setup as it is then, whatever a peer brought in meanwhile, and every
command-line run is a start: a scheduled `--reconcile-once`, or the `--command setup` that lists what
waits. Until then nothing counts as approved, and an approval or an edit made in between begins a new
record holding only what it approved. No start records the setup after that, and everything else
waits for review.

An edit made here approves what it changed. In the editor and with `--command edit --in`, it is
refused when the setup moved on since the text was read (start again from the setup as it is now),
and when it changes a command that waits for approval or carries one somewhere else: under a renamed
machine or on a new system, with or without a shell of its own. A file given to
`--command edit --in` without its `controller` block also needs `--hash HASH`, the setup it was
written against. A hand edit of the file that keeps its `controller` block is never refused, because
it is already written when it is read: it applies, approves what it changed, and leaves a waiting
command it changed or carried waiting. A file put in place without its `controller` block becomes a
setup of its own: on a device that applies none it is the first setup there and approves what it
holds, and over a setup that is applied it approves nothing, so whatever it changed waits for review.
Importing a setup into a device that has none approves everything in it, after showing all of it.
Nothing that arrives from another peer approves anything, and neither do merges.

`--approve-launch` takes the hash the listing printed, and approves nothing once the setup is no
longer that document, so a revision adopted in between is never approved unseen.

```
legion-control --command setup                                   # what waits, beside what was approved
legion-control --command setup --approve-launch 'machines[tower].systems[linux]' --hash HASH
legion-control --command setup --approve-launch all --hash HASH
legion-control --command setup --revert-launch 'machines[tower].systems[linux]'
```

`--reconcile-once` exits 1 while anything waits, as it does while two copies need a decision.

## Private settings

A key path, an ssh alias and which machine this device happens to be are true here and nowhere else,
so they live in `bindings.json` and are never published.

```
legion-control --command bindings \
  --device "Desk tower" \
  --self tower --system linux \
  --local-agent "node /home/me/.legion-control/agent/src/index.mjs" \
  --identity ~/.ssh/legion-control_ed25519 \
  --site flat
legion-control --command bindings --machine legion --alias legion-windows-lan
```

`--self` says this device is one of the machines in the setup. With `--local-agent` it is then
driven by spawning that command here rather than by an ssh loop back to itself; the argv is run as
it stands, with no shell and nothing to quote. Without it, it is dialled like any other machine.

`--site` matters where two homes sit behind the same router defaults. A `192.168.50.` address
cannot tell them apart, so a prefix match is only ever a hint: when two sites match, this app says
the location is unconfirmed rather than picking one, and a magic packet is only sent from here when
being on that network is more than a guess. What proves which machine answered is the pinned host
key, which is checked on every connection either way.

## Waking a machine

A magic packet reaches a machine only from something already on its network, so there is an order:

1. from here, when this device is demonstrably on that machine's network
2. through each configured helper in turn, by running that helper's own wake action
3. nothing, and a line per helper saying why

Helpers are never woken to wake something else. A machine that is asleep is offered as a separate
step somebody can take, because a cascade is how the tower nobody wanted on ends up on all night.

A packet is never evidence. Nothing acknowledges one, so the only thing this app accepts as a wake
is the machine's own agent answering afterwards.

## Trust

The release public key is compiled into the binary from `contract/release-public-key.pem` and read
from the assembly, never from disk. Nothing signed is installed unless it verifies against it:
this app's own updates, and the agent bundle a release build carries. A normal build carries no
agent bundle and says so instead of offering an install that cannot work.

Host trust is stored privately in `host-identities.json`. Confirm the operating systems that use
an address and assign each displayed fingerprint to an OS. A second OS can be approved only into
an empty local reservation; a third unknown key is refused. Shared setup edits cannot add trust
reservations. Existing OpenSSH pins remain in place and must be assigned explicitly before another
OS is enrolled. The app preserves custom host-key aliases and pin files; proxy or certificate
configurations it cannot inspect remain under native SSH management.

`--command trust --machine ID` displays and saves the exact offered keys. After independent
verification, `--command trust --machine ID --accept --system linux --trust-systems linux,windows`
confirms the local OS list and approves that saved scan. For an existing single-OS pin set, use
`--legacy-system windows`; the GUI supports assigning individual existing fingerprints.

The native window includes machine and service schedule editors, with automatic/inherit, pause,
and maintenance-window controls plus effective-policy readback. Setup import accepts a temporary
host, port, user, shell and agent argv before any shared setup exists. Private local argv is edited
as a JSON array to preserve paths containing spaces. Wake-into-system waits for authenticated
readiness, then asks before starting the tracked boot operation.

Service setup reads the complete agent configuration on demand, offers service templates and
an Add AI tool chooser, validates a preview, then saves the exact validated document with its
expected hash. Automatic updates start off for a newly added AI tool; unsupported and manual
tools show their availability reason. Restricted SSH sessions cannot edit service configuration.
Configured telemetry appears only when the agent sends named readings. Unavailable values stay
unavailable, and keeping telemetry history is a separate opt-in setting.

Use `--local-agent-json '["node","C:\\Program Files\\Legion\\bin\\launcher.mjs"]'` with
the `bindings` command when argv contains spaces. The complete array is preserved.
Agent installs require authenticated status and confirm the installed version afterward; legacy
bootstrap is limited to recognized layouts or an explicitly supplied `--base` directory.

## App updates

The window checks at startup and when it becomes active, with a fifteen-minute throttle. A timer
also checks every six hours while the window is active; an unfocused window catches up when
activated. Closing the window stops this timer. The local `checkForAppUpdates` preference in
`desktop-settings.json` defaults to `true`; a manual check is always available in **This device**.

An available release appears above machine navigation as **Review update**, so it remains reachable
while scrolling or switching machines. A transient failure keeps the previous verified offer
visible. Changing the configured release repository clears it immediately and invalidates an open
review, even if the repository is then changed back.

**Review update → Download and verify** downloads the exact artifact for this OS and architecture
and checks its size and hash against the signed manifest. **Install and restart** explicitly starts
the replacement helper. The previous build is retained and restored if the new window cannot
render. Checks do not download or install an app automatically.

Headless invocations do not run an update-check timer. Check or explicitly install from the CLI:

```
legion-control --command app-update
legion-control --command app-update --yes
```

The second command verifies and stages the update, then starts the helper. It reports completion
as pending until the replacement launches; the helper opens the graphical app. On a Pi without a
desktop session, verify the signed manifest and archive, then replace the complete installation
directory instead of using the restart helper, whose health check requires a rendered window.

## Building

```
./build.sh              test, then publish all three runtime targets into ../dist
./build.sh test         just the tests
./build.sh linux        Legion-Control-linux-x64.tar.gz and Legion-Control-linux-arm64.tar.gz
./build.sh windows      just Legion-Control-windows-x64.zip
```

It uses `$LEGION_DOTNET` when set, the pinned toolchain under
`~/.local/share/legion-control-toolchains/dotnet10` when it is there, and `dotnet` from the path
otherwise. All three archives are self-contained and are named exactly as the signed release manifest
names them, because the update path picks its own artifact by exact name and never by shape.

## Checking a real window from somewhere else

```
legion-control --screenshot /tmp/window.png --quit-after 6
```

Renders the real window with the real models behind it and exits. It exists for looking at a
machine that only has ssh, and it drives nothing: there is no UI automation here.

For process diagnostics, set `LEGION_CONTROL_PROCESS_TRACE` to an absolute log path. It records
process timings, stdin closure and stream character counts, without recording command arguments
or stream contents. `ProcessDiagnostic` is a standalone console harness using the production
runner; it accepts a request JSON path and trace path. Requests contain `executable`, `arguments`,
`timeoutSeconds`, and an optional `inputFile`. SSH calls without a payload use `-n`; calls that
upload a configuration or bundle retain stdin.
