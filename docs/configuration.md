# Configuration

Legion Control is an agent plus four client apps that agree on one contract. None of them knows
anything about your machines until you tell them. This page is the whole of what you can tell
them.

There are three configuration files:

| File | Lives on | Describes | Shared |
| --- | --- | --- | --- |
| **agent config** `~/.legion-control/config.json` | every controlled machine, one per system | what runs *here*: which system this is, which services to look after, when they may be updated unattended, how to switch boot targets, how to sleep, which extra actions to offer | no, per machine |
| **controller config** | every device an app runs on | which machines exist, where they are, how to reach them, how to wake them, and which systems each one can boot into | **yes** — every device carries the same document, and the machines pass it between them |
| **bindings** | every device an app runs on | what is true of *this device only*: which machine it is, its keys, its ssh aliases, which site it is on | no, and never published |

All three are plain JSON. All three are validated on load. The agent config is validated strictly:
a value that does not make sense is a **problem**, not something to fall back from, and a config
with problems stops the agent changing anything on the machine until it is fixed. None of them is
ever written by an installer.

Where the two client-side files live:

| Client | Controller config | Bindings |
| --- | --- | --- |
| macOS | `~/.config/legion-control/config.json` (`LEGION_CONTROL_CONFIG` overrides it) | `~/.config/legion-control/bindings.json` |
| Linux desktop | `~/.config/legion-control/config.json` | `~/.config/legion-control/bindings.json` |
| Windows desktop | `%APPDATA%\legion-control\config.json` | `%APPDATA%\legion-control\bindings.json` |
| Android | app-private storage; the same JSON pasted, fetched, or edited in the app | app-private settings |

Setting `LEGION_CONTROL_HOME` moves all of it — config, bindings, the generated key, the
`known_hosts` the client pins into, and the kept revisions — into that one directory. That is what
the tests use, and it is why a test run cannot reach your real setup.

## Vocabulary

- A **machine** is a physical box: one network card, one hardware address, one thing to wake.
- A **system** is one operating system installed on a machine. A dual boot machine has two
  systems; only one of them is up at a time. Every system runs its own copy of the agent with its
  own config. A single boot machine has exactly one system.
- A **service** is something on a system that has a version, can be running or not, can be busy,
  and can be updated and restarted. Any npm package, any self updating desktop app, or anything
  you can describe with a few commands.
- An **action** is a named command you want a button for. No version, no health, no busy state of
  its own: just something to run.
- An **operation** is one attempt at changing something — an update, a restart, a boot, a sleep, an
  action, a whole scheduled cycle. It has an id, a phase, a result, and it outlives the connection
  that asked for it. `operations.md` covers them.
- A **site** is a network you are sometimes on. It exists so a client can work out whether it can
  reach a machine's LAN from where it is standing.
- A **peer** is any device running one of the apps. Every peer can read, edit and publish the
  setup; none of them is in charge.
- A device that is also one of the machines in the setup is **bound to itself**, and controls
  itself through the same commands, run locally instead of over ssh.

## The agent config

`~/.legion-control/config.json` (`C:\Users\<you>\.legion-control\config.json` on Windows). The
install base can be moved with `LEGIONCTL_HOME`. Every key is optional; the file itself is
optional.

```jsonc
{
  // The schema this document is written against. The current version is 3. An agent refuses a
  // config that declares a version newer than it understands, rather than guessing at settings
  // it has never heard of. Leave it out and the agent reads the document as best it can.
  "configVersion": 3,

  // Which system this is. The id is what the controller apps match against, and what the other
  // system on a dual boot machine names as a boot target. Defaults to the platform name
  // ("linux", "windows" or "mac") and "Linux" / "Windows" / "macOS".
  "system": { "id": "linux", "name": "Linux" },

  // When the scheduled maintenance cycle is allowed to act unattended on this machine. It does
  // NOT decide whether a person may press Update — see "Update policy" below. The older boolean
  // `autoUpdate` still works and means `updates.automatic`.
  "updates": {
    "automatic": true,
    "pauseUntil": null,
    "maintenanceWindows": [{ "days": ["mon","tue","wed","thu","fri"], "from": "03:00", "to": "05:00" }]
  },

  // The services to look after. See "Service kinds".
  "services": [ /* ... */ ],

  // How to boot into another system. Keys are the target system ids. See "Boot targets".
  "boot": { "targets": { /* ... */ } },

  // How to suspend this machine. See "Sleep".
  "sleep": { /* ... */ },

  // Extra buttons. See "Actions".
  "actions": [ /* ... */ ],

  // Optional explicitly configured measurements. Omit this to collect nothing.
  "telemetry": { "probes": [ /* ... */ ] }
}
```

Any key not listed here is rejected by name, with the list of keys that were expected. A typo in a
config is a mistake worth being told about, not a setting to ignore.

### Service kinds

Every service has an `id` (short, stable, used on the command line as `--service <id>`), a `name`
(what the apps show), a `kind`, and the probes below. The id must be letters, digits, dot, dash
and underscore only, starting with a letter or digit. The kind decides where versions come from and
how an update happens.

#### `npm`: a package installed globally with npm

```jsonc
{
  "id": "dashboard",
  "name": "Home Dashboard",
  "kind": "npm",
  "package": "@example/dashboard",
  // The dist-tag to follow. The update installs whatever the tag points at. Default "latest".
  "channel": "latest",
  // Anything the tag resolves to must match this, or the update refuses. Guards against a
  // malformed dist-tag being handed to npm install. Default accepts any semver-shaped string.
  "versionPattern": "^\\d+\\.\\d+\\.\\d+$",
  // The global prefix the package lives under. Default: the platform's usual one, probed if
  // that is empty.
  "npmPrefix": null,
  // Packages allowed to run install scripts on npm 12 and newer.
  "allowScripts": ["node-pty"],
  "process": { "type": "systemd-user", "unit": "dashboard.service" },
  "health": { "type": "http", "port": 8080, "path": "/healthz" },
  "busy": { "type": "http", "port": 8080, "path": "/status", "busyWhen": "jobs.running" }
}
```

The update cycle for this kind: resolve the tag, compare with the installed `package.json`
version, check busy, pre-warm the npm cache, take the operation lock, **check busy again**, drain
if a drain command is configured, stop the process and poll until it is really gone, install,
verify the installed version, start, wait for health. Any failure reinstalls the previous version
and reports a rollback. The second busy check is the one that matters: cache warming can take
minutes, and the answer from before it is stale by the time anything is stopped.

#### `app`: a desktop app that updates itself

For apps that download their own updates and apply them when they quit (electron-updater and
similar). The agent never downloads anything; it notices that a build is staged and, when nothing
is busy, asks the app to quit and starts it again. macOS only.

```jsonc
{
  "id": "editor",
  "name": "Example Editor",
  "kind": "app",
  "path": "/Applications/Example Editor.app",
  "bundleId": "com.example.editor",
  // Where the app stages a downloaded build: <updaterCacheDir>/pending/update-info.json
  "updaterCacheDir": "~/Library/Caches/example-editor-updater",
  // How to read the version out of the staged file name. The first capture group is the version.
  "stagedFilePattern": "^Example-Editor-(.+)-(?:arm64|x64|universal)\\.(?:zip|dmg)$",
  // Where the newest version is announced, for reporting only. Nothing is installed from it.
  "latest": { "type": "github-releases", "repo": "example/editor", "prerelease": false },
  "health": { "type": "http", "port": 8080, "path": "/healthz" },
  "busy": { "type": "http", "port": 8080, "path": "/status", "busyWhen": "busy" }
}
```

`process` is implied for this kind: running means a process whose executable sits inside the
bundle exists, stop means asking the app to quit through AppleScript, start means `open -a`. The
quit is asked for and never forced. On a system that is not macOS the process type falls back to
`none`, so the config still parses and the service is simply not actionable there.

#### `command`: anything you can describe with commands

```jsonc
{
  "id": "media",
  "name": "Media Server",
  "kind": "command",
  // Each of these is an argv array, never a shell string. Optional ones may be left out.
  "installedVersion": ["mediaserver", "--version"],       // stdout, first line, trimmed
  "latestVersion": ["sh", "-c", "curl -fsS https://example.com/media/latest.txt"],
  "update": ["sudo", "-n", "/usr/local/libexec/media-update"],
  // The postcondition. Exit 0 means the update really took.
  "verify": ["systemctl", "--user", "is-active", "--quiet", "media.service"],
  // Run when the postcondition fails, to put the previous state back.
  "rollback": ["sudo", "-n", "/usr/local/libexec/media-rollback"],
  // Require the version read after the update to equal the target. Defaults to true whenever
  // both installedVersion and latestVersion are configured, because where equality is
  // meaningful it is the strongest postcondition available.
  "requireVersionMatch": true,
  "process": { "type": "systemd-user", "unit": "media.service" },
  "health": { "type": "http", "port": 8096, "path": "/health" },
  "busy": { "type": "command", "command": ["mediaserverctl", "idle"] }
}
```

An update command that exits 0 has proved nothing. The first version of the agent accepted
whatever version it read afterwards, so an updater that did nothing at all was reported as a
completed update. Now a `command` service update succeeds only when one of these holds:

- `requireVersionMatch` is on and the version read after the update equals the resolved target; or
- a `verify` command exits 0.

With neither, the config load warns that the result of the update cannot be checked. The version
reported as `to` is always the version actually read afterwards, never the target substituted for
it.

Without `latestVersion` the service reports that its version was not checked, and the scheduled
cycle leaves it alone: running an unconditional `update` every fifteen minutes would stop and
start the service each quarter hour for nothing. Without `update` the service cannot be updated
at all, only restarted, and the scheduler says so by name rather than silently skipping it.

### Editing services and using templates

The service editor uses an unrestricted administrative connection to read the full machine-local
configuration. It offers command, existing systemd user unit, and npm templates. These are drafts:
fill in their package, unit or command fields and choose a busy probe before saving. Choosing
`busy.type: "none"` explicitly permits work without busy protection; leaving the choice out is
refused. Saving configures the agent's existing adapters. It does not install a package, create a
unit, or run a setup command.

When the agent offers application profiles, each row reports its platform, whether the app was
detected, the supported update method, and an `available`, `not-installed`, `manual`, or
`unavailable` state with an explanation. A detected app may still need its own GUI to update;
that is a manual update path, not a working headless updater. Profiles use supported official
methods and produce drafts with automatic updates disabled. Review the draft and enable automation
only where the reported method fits your installation. The generic templates remain available
when no profile applies.

The command sequence is:

1. `service-config get` returns `document`, an opaque `hash`, `templates`, and
   `limits.maxBytes` (1048576). The hash is always present, including when the file is missing
   and the editor is offered an inert seed. Retain the returned hash exactly.
2. `service-config validate --stdin` reads `{ "expectedHash": "…", "document": {…} }`.
   The full document must have `configVersion: 3` and `services: []` or a populated service array.
   Validation returns added, removed and changed service ids, whether other settings changed,
   and any warnings. It writes nothing.
3. After reviewing that preview, `service-config set --stdin` sends the same proposal. The agent
   validates again and saves only if the configuration still matches `expectedHash`. A conflict
   returns `stale-revision`; reload and review the changes before saving again.

The one MiB limit includes the request envelope. Input is bounded to five seconds idle and fifteen
seconds total. All three commands refuse restricted keys, including `get`, because a local config
can contain private command arguments. Validation replies name fields without echoing their values.
This editor changes `config.json`; publishing the shared machine list with `config set` remains a
separate operation against `controller.json`.

### Probes

**`process`**, how the service is started, stopped and looked at:

| type | fields | notes |
| --- | --- | --- |
| `systemd-user` | `unit` | Linux. `systemctl --user start/stop/is-active` |
| `systemd-system` | `unit` | Linux. `sudo -n systemctl start/stop/is-active` |
| `scheduled-task` | `task`, `match` | Windows. `Start-/Stop-ScheduledTask`; running is decided by a process whose command line contains `match` (default: the npm package root) |
| `app` | | macOS, implied for kind `app` |
| `command` | `start`, `stop`, `running` | argv arrays, at least one required. `running` is judged by exit code 0 |
| `none` | | nothing to start or stop, so nothing that can be down: reported as running, and not restartable |

**`drain`** is a service-level command run after the operation lock is taken and before the service
is stopped, for a service that can be told to finish what it is doing and stop accepting new work.
Checking is not preventing: the busy gate notices work in progress, but only the service itself can
refuse to start more. There is no generic way to drain a process, so nothing happens unless you
configure one.

```jsonc
"drain": { "command": ["mediaserverctl", "drain"], "timeoutSeconds": 30 }
```

**`health`**, whether the service is answering:

| type | fields |
| --- | --- |
| `http` | `port`, `path` (default `/`), `host` (default `127.0.0.1`). Healthy means HTTP 200 |
| `command` | `command`, exit code 0 means healthy |
| `none` | healthy whenever the process is running |

**`busy`**, whether something would be lost by stopping the service. This gate sits in front of
every disruptive action and it fails closed: a probe that cannot be read counts as busy.

| type | fields | notes |
| --- | --- | --- |
| `command` | `command`, `staleHours` | exit code 0 means idle, anything else means busy. If stdout is a JSON object with `busy` and `reason`, those are used |
| `http` | `port`, `path`, `host`, `busyWhen` | GET the URL; busy when the JSON body's `busyWhen` path (dotted) is truthy |
| `t3-sqlite` | `home`, `staleHours` (default 6), `allowMissing` | counts running and pending turns and pending approvals in T3 Code's state database |
| `none` | | declared never busy |

**There is no default.** A service with no `busy` block at all is *unmonitored*, which is not the
same as idle: it probes as unknown, and unknown fails closed, so disruptive actions on it are
refused until you say which you meant. Writing `"busy": {"type": "none"}` is how you say "this
service has no work worth protecting", and the point is that it has to be said out loud. The
config load reports the unmonitored service as something to fix, and `doctor` repeats it.

Two more rules that changed, both in the same direction:

- **A running unit of work never ages out on its timestamp alone.** The old behaviour was that a
  running turn older than `staleHours` stopped blocking, which is exactly wrong: a long job is the
  case the gate exists for. It is now treated as abandoned only with independent evidence — the
  service process is not running, or the process started after the work did, since a job cannot
  outlive the server that was running it. `staleHours` still applies to *pending* work, which a
  crashed session really can leave behind forever.
- **A missing state database is unknown, not idle.** `allowMissing: true` on a `t3-sqlite` probe
  says "this machine may legitimately never have run the product", and only then is absence read
  as idle. Absent and misconfigured look identical from the agent's side, and the safe reading of
  the ambiguity is the one that does not throw work away.

**`endpoint`**, where the service is supposed to be reachable from outside:

```jsonc
"endpoint": { "url": "https://dashboard.example.com/healthz", "timeoutMs": 8000, "expectStatus": 200 }
```

Never probed by an ordinary status poll. Turning the thing you press twenty times a day into an
Internet round trip would make it slower and flakier for nothing. It is checked by
`doctor --deep`, where the question actually being asked is whether anything outside the machine
can get to the service. A relay process being up does not prove that; this does.

**`relay`** names a tunnel process that has to be up for the service to count as reachable from
outside: `{ "type": "cloudflared" }`, or `{ "type": "none" }`. Leave it out when there is none.

**`lockFile`** on a service names the per-service maintenance lock the update takes while it
works, for the case where something else on the machine — a watchdog task, say — already honours a
particular file. Default: `<base>/update.lock`. This is separate from, and taken inside, the
machine-wide operation lock described in `operations.md`.

### Optional measurements

Telemetry runs only the commands explicitly listed in the agent's own config. It does not discover
sensors or collect anything when `telemetry` is absent, null, or has an empty `probes` array.

```json
{
  "telemetry": {
    "probes": [
      {
        "id": "queue-depth",
        "name": "Queue depth",
        "unit": "jobs",
        "command": ["node", "/opt/demo/read-queue-depth.mjs"],
        "timeoutSeconds": 1
      }
    ]
  }
}
```

That example requires your own read-only measurement script. A command must print one finite
number; decimal and exponent notation work. Empty output, multiple lines, `NaN` and `Infinity`
are failures. There are at most eight probes, with unique service-style ids up to 64 characters.
`name` defaults to the id (maximum 120 characters), `unit` defaults to empty (maximum 24), and
`timeoutSeconds` defaults to 1, with a range greater than 0 through 2. Names and units cannot
contain control characters. Command argv entries must be nonempty strings without NUL bytes.

At most two probes run concurrently within the existing status deadline. `status.metrics` appears
only when probes are configured. Each row has `id`, `name`, `value`, `unit`, `checkedAt`, and
`error`. Zero is a measured value. An unavailable reading is null with an explanation; a probe
that could not start before the deadline also has a null check time. These readings do not change
the busy gate or establish that an update, reboot or wake succeeded.

### Update policy

Turning automatic updates off used to mean nobody could update anything: the same switch answered
"should the timer install things unattended?" and "may this person press Update?", and the only
way past it was `--force`, which also skips the busy gate. The setting that exists to make a
machine more careful made the only available action less careful.

There are now three separate things:

- **Eligibility** — may the scheduler act unattended right now? Decided by the system switch, the
  per-service switch, a pause, and a maintenance window.
- **Request** — a person asked. Always allowed wherever there is an update path at all.
  Eligibility is never consulted.
- **Override** — may this run interrupt work in progress? That is `--force`, and it is the only
  thing `--force` means. It never breaks the operation lock, never overrides an invalid config,
  and never turns an unknown result into a success.

The same three keys appear at the top level, for the machine, and on each service. A service that
leaves one out, or sets it to `null`, inherits the machine's.

```jsonc
// Machine-wide.
"updates": {
  "automatic": true,
  // ISO timestamp. The scheduler waits until then. Manual requests are unaffected.
  "pauseUntil": null,
  // Local wall-clock time, because a maintenance window is a statement about when the person who
  // owns the machine is asleep. An empty list means any time. A window whose end is before its
  // start wraps past midnight and belongs to the day it started on: "fri" 22:00-04:00 covers
  // Friday evening and the early hours of Saturday, not Friday's own small hours.
  "maintenanceWindows": [{ "days": ["mon","tue","wed","thu","fri"], "from": "03:00", "to": "05:00" }]
}
```

```jsonc
// On a service. null, or absent, inherits.
"updates": {
  "automatic": false,
  "pauseUntil": null,
  "maintenanceWindows": null
}
```

The machine-wide `automatic` is a **master stop**: with it off, the scheduler does nothing, and a
service cannot turn itself back on. Off on the machine has to mean off. With it on, a service that
says `false` takes itself off the schedule, so one noisy service can come off without taking the
machine off. Manual requests are unaffected either way.

A pause can come from the config file or from `auto-update pause`, which writes to the service's
state rather than rewriting your config. Whichever lasts longer wins: someone who paused for four
hours and then edited the config to pause for a day meant the day. The status reply says which
side the pause came from, so a client can offer the right "resume".

Every service the scheduler declines to touch is declined with a reason code and a sentence, so
the apps can say "paused until Tuesday" or "only updates between 03:00 and 05:00" rather than
"nothing happened". A service with no update path at all is declined by name too, instead of being
silently skipped forever while the app shows it as out of date.

The older top-level boolean `autoUpdate` is still read and written as an alias for
`updates.automatic`, so a config from an earlier version keeps working.

`status` and `policy` report the policy already resolved, which is what the apps draw the
maintenance rows from: for the machine, whether a window is open now and when the next one opens,
and the last cycle; for each service, the effective `automatic` with whether it was inherited, the
pause in force, whether it is eligible right now, and the reason if it is not.

#### Ordering within a cycle

Two optional keys on a service's `updates` block, beyond the contract's three:

```jsonc
"updates": {
  "automatic": null,
  // Lower first. Default 0.
  "order": 10,
  // Real dependencies, not hints: everything listed here is updated first, so a front end comes
  // down after the thing it fronts and comes back up before anything asks for it. A cycle in the
  // graph is reported rather than resolved — there is no correct order, and picking one quietly
  // would be worse than saying so.
  "after": ["dashboard"]
}
```

Leave both out and the cycle walks the services in the order they appear in the file.

### Boot targets

`boot.targets` is keyed by the id of the system to boot into. Each entry says how to arm the
firmware from *this* system. Arming is always read back and verified before anything reboots; a
reboot that was not verifiably armed is refused, because rebooting an unarmed machine just returns
to the same system and the apps would sit waiting for one that never comes.

| method | platform | fields | what it does |
| --- | --- | --- | --- |
| `efi-bootnext` | Linux | `match` (regex against the entry description) | `efibootmgr --bootnext <entry>`, entry resolved by description on every call, never cached |
| `clear-bootsequence` | Windows | | `bcdedit /deletevalue {fwbootmgr} bootsequence`, so the next boot falls through BootOrder |
| `bootsequence` | Windows | `entry` (a `{guid}` or `{bootmgr}`) | `bcdedit /set {fwbootmgr} bootsequence <entry>` |
| `grub-reboot` | Linux | `entry` | `grub-reboot <entry>`, read back from `grub-editenv list` |
| `command` | any | `arm` (argv), `verify` (argv, exit 0 means armed) | whatever you say. Without `verify` this is the one arm path that is not read back, the config load warns about it, and the reply says so |

```jsonc
"boot": {
  "targets": {
    "windows": { "method": "efi-bootnext", "match": "^Windows Boot Manager\\b", "name": "Windows 11" }
  },
  // A constrained helper is the recommended way to reboot with elevation: one program that does
  // exactly one thing. See security.md — the alternative is a sudo rule for a shell, which is a
  // root shell wearing a hat.
  "rebootHelper": ["sudo", "-n", "/usr/local/libexec/legion-reboot"]
}
```

and on the Windows side of the same machine:

```jsonc
"boot": { "targets": { "linux": { "method": "clear-bootsequence", "name": "Linux" } } }
```

`boot.reboot` overrides the reboot command outright (argv). A `name` on a target is optional and
is only used when the controller has no name for that id.

A `command` target **must** have a `verify`. An arm that cannot be read back is an arm that cannot
be trusted, and rebooting on the strength of it lands you back in the system you were already in
while the apps wait for one that never comes. `--force` does not get past this: force overrides the
busy gate, and an unverifiable arming is not a busy problem.

A config with no `boot` block offers no boot targets. Nothing is synthesised from the platform: a
Linux machine that is not dual boot should not be told it can reboot into Windows. The one
exception is a config still using the pre-`services` keys, which is migrated with its old
assumptions intact and a warning saying so.

### Sleep

```jsonc
"sleep": {
  // Commands to run first, each an argv array. For anything on the machine that holds sleep off
  // while an ssh session is open.
  "before": [["powershell", "-NoProfile", "-Command", "Stop-ScheduledTask -TaskName MyGatekeeper"]],
  // Seconds to wait after the "before" commands, for whatever they stopped to actually let go.
  // Only waited when there was something to run first.
  "settle": 2,
  // The suspend command itself. Defaults: `sudo -n systemctl suspend` on Linux, `pmset sleepnow`
  // on macOS, and on Windows the first of `tools` that exists, run as `<tool> -d -t 3 -accepteula`.
  "command": null,
  // Windows only: psshutdown candidates, best first. The built in rundll32 and .NET suspend calls
  // are silently vetoed on some machines, which is why this is a list of tools resolved at run
  // time rather than one assumed path.
  "tools": ["C:\\Tools\\PSTools\\psshutdown64.exe"]
}
```

The agent only ever learns that the suspend command was accepted; the machine goes down underneath
it. A `sleeping` result means accepted, not confirmed, and the apps say so.

### Actions

```jsonc
"actions": [
  {
    "id": "media-restart",
    "name": "Restart Media Server",
    "command": ["systemctl", "--user", "restart", "media.service"],
    // Shown before running. Leave it out for actions that need no confirmation.
    "confirm": "Any stream playing right now stops.",
    // Refuse while any service is busy, unless forced.
    "busyGated": true,
    "timeoutSeconds": 60
  }
]
```

Actions appear in `status` and run with `legionctl run <id> [--force]`. The reply carries the
result, the exit code and the first lines of output, and all four clients render that output —
an action that ran and printed something is worth showing.

An action has **either** a `command` or a `wol` block, never both.

#### `wol`: send a wake packet from this machine

```jsonc
"actions": [
  {
    "id": "wake-tower",
    "name": "Wake the tower",
    "wol": {
      "mac": "AA:BB:CC:DD:EE:FF",
      // Where to send it. Usually the broadcast address of the LAN this machine is on.
      "broadcast": ["192.168.178.255"],
      // Default [9, 7].
      "ports": [9, 7],
      // How many datagrams per address. Default 3, maximum 10. UDP is not reliable and a
      // sleeping network card gets one chance to notice.
      "repeats": 3
    }
  }
]
```

The agent sends the standard 102-byte magic packet itself, from an unbound UDP socket with
broadcast enabled. No `wakeonlan` binary, no shell, nothing to install: any machine already
running the agent can wake its neighbours. The result names how many packets went to which
addresses; if not one datagram left the machine, the action fails and says which address refused.

This is what makes a small always-on machine — a Pi, a router that runs the agent, a desktop that
happens to be up — useful as a wake helper for everything else on its network. See "Waking from
somewhere else" for how a client picks one.

A `wol` action is not busy-gated by default: sending a packet interrupts nothing. It takes the
operation lock like every other action.

`status.actions[]` says which kind each action is, so a client can label a wake action as a wake
action rather than as a generic button.

### There is no key setting

Nothing in this file decides what the agent will trust. The one Ed25519 public key that signs
releases is compiled into the agent and into every client, from `contract/release-public-key.pem`,
and a signed agent bundle is verified against that key and nothing else.

That is deliberate. `config.json` is a file: anyone who can write it could otherwise nominate their
own signer and have the machine install their own build, which would make writing a config
equivalent to running arbitrary code. `security.md` covers the whole trust model.

### Legacy keys

A config written for the first version of the agent (`port`, `channel`, `allowScripts`,
`npmPrefix`, `staleTurnHours`, `t3AppPath`, `t3AppBundleId`, `updaterCacheDir`) still works: when
`services` is absent, those keys describe one T3 Code service exactly as before, with the
platform's default process type, the `t3-sqlite` busy probe, the strict nightly `versionPattern`
the first version enforced, and on Windows the `%LOCALAPPDATA%\T3Code\update.lock` lock file. The
load reports this as a migration to make, with the fix named, and the old keys keep working until
you make it. A new-style config gets none of those defaults: name the unit, the task and the lock
file yourself.

### When the config is broken

The old behaviour collapsed missing, unreadable and malformed into the same empty object, and that
empty object enabled automatic updates. A truncated edit on a machine where updates were
deliberately off could silently turn them back on. That is fixed, and the three cases are now
three cases:

| State | What the agent does |
| --- | --- |
| The file is not there | The agent is **inert**: no services, automatic updates off, no boot targets. It answers diagnostics and permits signed agent installation or administrative configuration. Service and power operations require explicit configured services and targets; a missing file does not invent them |
| The file is there and parses, but has problems | The agent still reports what it understood, and refuses every command that would change anything until the problems are fixed |
| The file is there and cannot be read or parsed | Nothing is guessed. The error names the file, and points at `<base>/config.last-good.json` — the last copy that loaded cleanly — with the time it was kept |

A known-good copy is maintained by configuration writes and non-read-only loads. Status and other
read-only commands do not rewrite it. `legionctl doctor` prints problems with a fix line for each.

## The controller config

The same document on every client. The Mac watches its file through the directory, so an editor's
write-and-rename is picked up and a save lands in the window with no restart. A config that does
not parse or validate never replaces one that did: the machines stay and the reason appears on the
setup page. On the phone, Apply stores nothing until the document passes.

Validation is strict where a fallback would be a guess: ids must be unique, every machine needs at
least one system and either `ssh` or an endpoint, every system needs a non-empty `agent` argv, and
a `wake` block needs a parseable hardware address. Missing names fall back to ids, missing ports
to 22, an unknown `platform` to `linux`.

This document is **shared**. Every device carries the same bytes, every device may edit it, and
the machines pass it between them. Nothing device-specific belongs in it — see "Private bindings"
below for where your key paths and ssh aliases go instead.

```jsonc
{
  "version": 1,

  // Which setup this is, and how far it has come. Maintained by whichever device last edited it.
  // See "Sharing the setup between devices"; you do not normally hand-edit this block.
  "controller": {
    "id": "setup-3f9c2a17-8d41-4e6b-b0c9-5a2e7f14d803",  // the SETUP, not a device
    "name": "Home setup",
    "revision": 12,
    "updatedAt": "2026-09-05T09:00:00Z",
    "source": "mac",                       // which kind of client wrote it
    "device": "Robert's MacBook",          // and which device, for the "who changed this" line
    "lineage": ["9b1e…", "c712…"]          // hashes of the documents this one descends from
  },

  // Where the LANs are. Optional; a setup that never leaves one network does not need it.
  "sites": [
    { "id": "flat", "name": "The flat",
      // A device whose own address starts with one of these is PROBABLY on this site.
      "lanPrefixes": ["192.168.178."],
      // Default broadcast for machines here, when a machine does not give its own.
      "broadcast": ["192.168.178.255"] }
  ],

  "machines": [
    {
      "id": "tower",
      "name": "Tower",
      "site": "flat",           // optional; must name a site above when sites exist
      "alwaysOn": false,        // optional hint. Only changes what the apps say about helpers.

      // Addresses to dial directly, in preference order. "kind" is "lan" (broadcast domain shared
      // with the machine, so a direct wake can work) or "remote" (a tunnel or VPN address).
      // "system" is a routing hint: an address that belongs to one system saves a wasted round
      // trip guessing the other one.
      "endpoints": [
        { "id": "tailnet-linux", "kind": "remote", "host": "100.64.0.10", "port": 22, "user": "me", "system": "linux", "label": "Tailnet, Linux" },
        { "id": "lan", "kind": "lan", "host": "192.168.178.40", "port": 22, "user": "me", "label": "Flat LAN" }
      ],

      // Wake on LAN. Leave the block out for a machine that cannot be woken.
      "wake": {
        "mac": "AA:BB:CC:DD:EE:FF",
        "broadcast": ["192.168.178.255"],
        "ports": [9, 7],
        // Polled after the packet, to tell when the machine is back.
        "probe": { "host": "192.168.178.40", "port": 22 },
        // Machines that can send the packet when you cannot, tried in order.
        // See "Waking from somewhere else".
        "helpers": [
          { "machine": "pi-flat",     "action": "wake-tower" },
          { "machine": "router-flat", "action": "wake-tower" }
        ],
        // Compatibility: the same as helpers[0], written by editors so a 1.2 client still works.
        "helper": { "machine": "pi-flat", "action": "wake-tower" }
      },

      // Every system installed on the machine. "id" must match the agent's system.id on that
      // system. "agent" is the argv run over ssh.
      "systems": [
        { "id": "linux", "name": "Linux", "platform": "linux", "shell": "posix",
          // The key for this system is restricted to the dispatcher: see security.md. Clients send
          // a plain argv list rather than a line for a shell to parse. Default false.
          "restricted": true,
          "agent": ["/usr/bin/node", "/home/me/.legion-control/agent/src/index.mjs"] },
        { "id": "windows", "name": "Windows 11", "platform": "windows", "shell": "powershell",
          "agent": ["node", "C:\\Users\\me\\.legion-control\\agent\\src\\index.mjs"] }
      ]
    }
  ],

  // Where the apps look for their own updates. Default: this project's releases.
  "appUpdates": { "githubRepo": "x1f4r/legion-control" }
}
```

`platform` is `linux`, `windows` or `mac` and picks the icon and the wording. A `symbol` on a
system overrides the icon on the Mac with any SF Symbol name.

The document stays at `"version": 1`. Everything added for multiple sites and multiple peers is an
extra key, and a client that has never heard of it ignores it — so a 1.2 app keeps reading a
document written by a 1.3 app. It will not use the new capabilities, but it will not choke.

### Rules the editors enforce

Errors block Apply and Publish; warnings are shown next to the field.

| | Rule | |
| --- | --- | --- |
| V1 | `controller.id` is a non-empty token, `revision` a whole number, `lineage` at most 32 distinct lowercase sha256 hashes | error |
| V2 | every `machine.site` names a site that exists; site ids are unique | error |
| V3 | every `wake.helpers[].machine` exists, is not the machine itself, and does not form a helper cycle; `action` matches the agent's id grammar | error |
| V4 | a helper at a different site from its target | warning: it only works if that action can reach the target's network some other way, such as a router |
| V5 | a machine with `wake` but no site, no `lanPrefix` and no helper | warning: only a device already on its LAN can wake it |
| V6 | an editor that writes `helpers` also writes `helper` as the first of them | — |
| V7 | keys the editor does not know are kept, not dropped | — |

V7 matters more than it looks. Four clients edit this document and they will not always be the
same version. An editor that rebuilt the file from its own idea of the schema would quietly delete
whatever a newer app had added. So every editor works on the parsed JSON and puts back what it did
not recognise.

### Deprecated keys

Three keys still work and are still read, but belong in your private bindings now:

| Key | Instead |
| --- | --- |
| `machine.ssh` | `bindings.machines.<id>.sshAlias` and `identityFile` |
| `machine.ssh.identityFile` | `bindings.identityFile`, or per machine |
| `local` | `bindings.self` and `bindings.localAgent` |
| `wake.lanPrefix` | `sites[].lanPrefixes` and `machine.site` |
| `wake.helper` | `wake.helpers` (the singular is kept as the first entry) |

Nothing strips them for you. Removing a key changes the document's bytes, which changes its hash,
which would look to every other device like an edit you did not make. The editors offer it as an
explicit choice — "move this device's private settings out of the shared setup" — and otherwise
leave them exactly where they are.

### Sites, and why a subnet is not a place

A site is a network you are sometimes on. It exists so a client can answer one question: *can I
reach this machine's LAN from where I am standing right now?*

```jsonc
"sites": [
  { "id": "flat",  "name": "The flat",   "lanPrefixes": ["192.168.178."], "broadcast": ["192.168.178.255"] },
  { "id": "attic", "name": "Attic house", "lanPrefixes": ["192.168.178."], "broadcast": ["192.168.178.255"] }
]
```

Those two entries are not a mistake in the example. Consumer routers hand out the same private
range everywhere, so being on `192.168.178.x` tells you almost nothing about *which*
`192.168.178.x` you are on. Legion Control therefore treats a prefix match as a **hint and never
as proof**:

- One site matches: that site is used, and the client says so.
- More than one site matches: the site is **unconfirmed**. The client does not pick the first one.
  It behaves as though it were off-site — which is the safe direction, since the worst case is
  going through a helper that works rather than broadcasting into a network you are not on.
- Set `currentSite` in your private bindings to say which one you are actually on. It is a routing
  hint only, and it is per device, because it is a fact about where *you* are.

What is never in doubt is the machine itself. Reaching a machine authenticates it against its
pinned ssh host key, so an address that happens to match another network's numbering cannot get you
talking to the wrong box — you get a host key mismatch, loudly. And a known remote address is never
rejected merely because some local prefix collides with another site.

### Waking from somewhere else

Wake on LAN is a broadcast. It reaches a machine on the same network segment and nowhere else, so
a client on a tunnel cannot wake anything — and a machine that is asleep is not running the tunnel
daemon either, which is why its remote address is useless for this.

The way round it is a machine that is already awake on that network. Give it an action that sends
the packet, and point the sleeper's `wake` block at it:

```jsonc
"wake": {
  "mac": "AA:BB:CC:DD:EE:FF",
  "helpers": [
    { "machine": "pi-flat",     "action": "wake-tower" },
    { "machine": "router-flat", "action": "wake-tower" }
  ],
  "helper": { "machine": "pi-flat", "action": "wake-tower" }
}
```

with, in the helper's own agent config, a native wake action — no third-party binary needed:

```jsonc
"actions": [
  { "id": "wake-tower", "name": "Wake the tower",
    "wol": { "mac": "AA:BB:CC:DD:EE:FF", "broadcast": ["192.168.178.255"] } }
]
```

`examples/agent-config-helper.json` is a whole config for such a machine: no services, no boot
targets, nothing but wake actions.

#### What a client actually does

1. **On the target's site** — send the packet directly, to `wake.broadcast` or the site's.
2. **Otherwise** — walk `helpers` in order. For each one whose last known state was not asleep, run
   its action. The first that reports it sent packets wins.
3. **Nothing worked** — say so, with one line per helper: unreachable, action failed, or asleep.

A helper is never woken automatically to serve as a helper. That would quietly turn the machine you
are trying not to leave running into a machine that is always running, which is the opposite of the
point. If a helper has a `wake` block of its own, the apps offer "wake it first" as a separate
thing you choose.

#### A sent packet is not a woken machine

UDP is fire and forget. The agent can tell you it put three datagrams on the wire; it cannot tell
you anything arrived, and neither can you. So the packet is never the end of it: readiness is an
authenticated poll of the target, at most every three seconds, and only a real reply counts. If the
direct broadcast produces no answer, the client moves on to the configured helpers rather than
declaring success.

Failing over between helpers is careful about a helper whose connection dropped mid-request. A
`wol` action is a pure packet send and is safe to repeat, so it simply retries. A general `command`
action is not: the client queries that same operation id on the helper when it can reach it again,
and only moves to the next helper once the first is a known failure or a known unknown. It never
re-runs an arbitrary command on a hunch — and it never retries a boot or a sleep without
reconciling the operation first.

`machine.alwaysOn` changes nothing except what the apps say. A wake path whose only helpers are not
marked always-on gets told, in the editor and in the failure message, that there is no always-on
helper for that site.

Once the machine is awake, booting it into a particular system is an ordinary boot request against
the machine itself, with the same confirmation and read-back rules as any other.

### `shell`, and why it matters

An ssh command is not a list of arguments. It is a **string** that the remote account's login
shell parses. The first version of this project dealt with that by requiring that nothing ever
need quoting — no spaces in any path, no shell characters in any id — and documenting the
restriction. That works right up until someone's home directory has a space in it.

`shell` says which shell the remote account logs in to, so the clients can quote for it:

| value | for |
| --- | --- |
| `posix` | sh, bash, zsh, fish and anything else that quotes with single quotes |
| `powershell` | Windows accounts whose OpenSSH shell is PowerShell |
| `cmd` | Windows accounts whose OpenSSH shell is cmd.exe |

Absent means "nothing here needs quoting", which is true of a plain `/usr/bin/node
/home/me/...` and false the moment a path has a space in it. Declare it. A system whose `agent`
argv contains a character that needs quoting and which declares no shell is refused by validation,
because the alternative is a command that means something different on the remote side than it did
on the client.

On Windows, which one you need depends on how the account is set up. A stock Windows OpenSSH
install runs `cmd.exe`, so `"shell": "cmd"` is right unless someone has set the registry's
`DefaultShell` to PowerShell, in which case it is `"shell": "powershell"`. The installers report
which shell the account actually logs in to, and `doctor` checks it. Nothing changes a machine's
default shell to suit a client.

A system whose key is restricted to the dispatcher takes `"restricted": true`. The forced command
means the agent parses a limited POSIX quoted argv list on every platform. Clients still quote
paths and arguments with that grammar; the original command is never evaluated by a shell. The
agent's status reports that a session arrived through the dispatcher, but the flag has to be in the
config for the very first request, before there is any reply to learn it from.

Beyond this file, everything the clients send is a token from a fixed grammar: ids, targets and
action names are validated on both sides before they go near a command line. A service id with a
semicolon in it is a config error, not a quoting problem to solve later.

### Reading an older agent

The apps accept an agent that predates the contract they were built for. A status without
`services` is read as one service built from its `t3` block; a status without `system` takes the
platform name as the system id; an agent that reports no boot targets at all is offered every
other configured system as a target, while one that reports an empty list is offered none. The
apps send flags only to an agent that has told them it understands those flags. That is what lets
the apps be updated before the machines are — and the machine section says outright which contract
the agent is speaking and offers to install a newer one.

## Private bindings

Everything that is true of *this device only* lives in a second file that is never shared, never
published, and never travels between machines:

| Device | Path |
| --- | --- |
| macOS, Linux | `~/.config/legion-control/bindings.json` |
| Windows | `%APPDATA%\legion-control\bindings.json` |
| Android | app-private settings |

Under `LEGION_CONTROL_HOME` — which the tests use — that path, the generated key, and the
`known_hosts` this device pins into all move together into that directory. A test run cannot touch
your real ssh setup.

```jsonc
{
  // The name this device writes into controller.device when it edits the setup.
  "deviceName": "Robert's tower",

  // Which machine in the shared document IS this device. Optional.
  "self": { "machine": "tower", "system": "windows" },

  // How to run the agent here without ssh. The argv is spawned directly — no shell is involved,
  // so there is nothing to quote and no "shell" key to set.
  "localAgent": { "argv": ["node", "C:\\Users\\me\\.legion-control\\agent\\src\\index.mjs"] },

  // Which site you are actually on, when the addresses cannot tell. See below.
  "currentSite": "flat",

  // The default key for every machine.
  "identityFile": "~/.ssh/legion-control_ed25519",

  // Where this device keeps its pinned host keys. Normally left out. The test harness sets it
  // through LEGION_CONTROL_HOME so an isolated run cannot touch your real known_hosts — and
  // entries are never removed automatically, in a test run or out of one.
  "knownHostsFile": "~/.ssh/known_hosts",

  // Per machine overrides. "sshAlias" names an entry in THIS device's ~/.ssh/config.
  "machines": {
    "attic-box": { "identityFile": "~/.ssh/id_attic", "sshAlias": "attic-box-lan" }
  }
}
```

`examples/bindings.json` is the same file ready to edit.

The identity used to reach a machine is the per-machine `identityFile`, else the device-wide one,
else the deprecated `machine.ssh.identityFile` in the shared document, else the client's own
generated key. An explicitly configured endpoint inherits it like any other dial — a key is not
dropped just because you named an address.

### This device as a machine

Setting `self` says "one of the machines in this document is me". The machine's section is then
titled with this device's own name, and with `localAgent` set every command against it is run by
spawning that argv here rather than by opening an ssh session to yourself.

Boot and sleep still work on your own device. They ask for the same confirmation as anywhere else,
plus the obvious sentence that this is the machine you are sitting at, and the outcome is read from
the operation record when it comes back — never guessed from uptime.

Android never has `self` or `localAgent`: a phone does not run the agent. Everything else on this
page applies to it unchanged, including editing and publishing the setup.

## Sharing the setup between devices

Writing the setup once per device is one time too many, so the machines carry it. Every agent keeps
a copy at `<base>/controller.json`. The agent does not interpret it: it never reads `machines`, and
it stores and serves the bytes it was given. The one part it does look at is the `controller` block,
because that is what lets it refuse a push that would lose somebody's work.

**Every device is a peer.** There is no authority device, no follower mode, no switch to make one
device special. The Mac, the phone and either desktop can all edit the setup and publish it, and
they all use exactly the same rules to decide what to do when two copies differ.

### Why revision numbers are not enough

The obvious design is a counter: higher revision wins. It works with one writer and it silently
loses data with two.

Two devices both hold revision 5. The laptop, at home, edits it to 6 and publishes to the machines
it can reach. The phone, out of the house, edits its own copy of 5 to 6 and publishes to the one
machine it can reach. Both edits are real. Later the laptop edits again to 7 and reaches that last
machine, which sees 7 > 6 and takes it. The phone's edit is gone, and nothing anywhere ever said so.

The number cannot detect that. What detects it is **ancestry**: not "which is bigger" but "did this
one grow out of that one".

### Lineage

So the document carries the hashes of the documents it grew out of:

```jsonc
"controller": {
  "id": "setup-3f9c2a17-8d41-4e6b-b0c9-5a2e7f14d803",
  "name": "Home setup",
  "revision": 12,
  "source": "mac",
  "device": "Robert's MacBook",
  "lineage": ["9b1e…", "c712…", "0a44…"]
}
```

- `id` is the **setup**, not a device. Every peer shares it. It is generated once, by whichever
  device first creates the document, and it never changes on an edit.
- `revision` still counts up, and is still only ever compared within one `id`. It is now for people
  to read, not for machines to decide with.
- `lineage` is up to 32 hashes of ancestor documents, newest first, each the sha256 of that
  document's canonical bytes.

Editing takes the document you have, bumps the revision, and puts the hash of what you started from
at the front of the lineage. Chains of offline edits work normally: each one remembers the one
before it.

### What an agent accepts

When a client publishes, the agent compares what it is being handed with what it holds:

| Situation | What happens |
| --- | --- |
| It holds nothing, or a copy from a 2.x client with no identity | stored |
| The pushed bytes are the bytes it already holds | nothing written, reported as a no-op |
| What it holds appears in the pushed document's lineage | stored — this is a fast-forward, the pusher is ahead. The revision must also be above the one it holds, since a descendant is by definition later |
| What is being pushed appears in the lineage of what it holds | refused as **stale**: the pusher is behind and should catch up |
| A different setup id | refused as a **conflict**, unless a person explicitly chose to replace |
| Anything else | refused as a **conflict**, marked divergent: both sides have real changes |

Revision order never decides acceptance. An agent will refuse revision 99 if it does not descend
from what that agent already has.

This is the whole safety property: **an agent will not overwrite a document unless the new one grew
out of the old one.** A concurrent edit cannot be lost quietly, because it cannot be overwritten
quietly.

### What a client does about it

On each successful status of a machine, the client compares the hash the machine reports with the
one it has applied:

| | What the client does |
| --- | --- |
| Same hash | Nothing. In sync. |
| The machine has nothing | Publish, automatically. |
| The machine's hash is in my lineage | The machine is behind. Publish, automatically. |
| Anything else | Ask the machine for its metadata once, and then: |
| — different setup id | **Ask a person.** "Replace that machine's setup with mine", or "adopt its setup here", with a preview of what changes. |
| — my hash is in *its* lineage | I am behind. Fetch, check the hash of what came back, apply automatically. |
| — neither descends from the other | **Ask a person.** This is divergence; see below. |

Automatic writes only ever happen along strict descent — publish when the machine is my ancestor,
fetch when I am the machine's ancestor. Descent has a direction, so two devices cannot take turns
overwriting each other. Everything else stops and waits for you.

A machine running an agent too old to carry a setup is simply left out of all this, and said to be
left out. It is not a conflict.

### When two people edited at once

Divergence means same setup, both sides changed, neither grew from the other. The apps show both
revisions, who wrote each and when, and a difference broken down per machine, per site, per
endpoint and per system — not a wall of JSON.

You get three choices:

- **Merge**, the default. Where the two branches came from a common ancestor the app still has, it
  takes whichever side changed each entry and only asks you about entries that both sides touched.
  Without a common ancestor it asks about every difference. The result descends from *both*
  branches, so publishing it makes every machine fast-forward and both edits survive.
- **Keep mine.** This makes a new revision whose lineage includes theirs, so it is accepted as an
  ordinary fast-forward. It never uses the replace override.
- **Take theirs.** Fetch and apply.

Each device keeps the last thirty documents it has applied, filed by hash, which is what makes the
three-way merge possible and what lets you look at what the setup used to be.

The lineage is capped at 32 entries, so two branches more than 32 revisions apart are reported as
divergence even if one is merely a long way behind. That errs toward asking a question, which is
the direction to err in.

### Canonical bytes

Both sides have to agree on what "the same document" means before they can agree on a hash. Most of
the trouble here was a missing final newline. The canonical form is:

- valid UTF-8 — anything else is refused before it is hashed
- one leading byte order mark removed
- CRLF and lone CR converted to LF
- ASCII whitespace stripped from both ends of the whole document, meaning exactly tab, newline,
  vertical tab, form feed, carriage return and space, not whatever each language calls "whitespace"
- exactly one trailing LF

The hash is the lowercase hex sha256 of *those* bytes, computed the same way in JavaScript, Swift,
Kotlin and C#. `contract/tools/canonical.mjs` is the reference implementation and
`contract/hash-vectors.json` the cases every client is tested against — no final newline, CRLF, a
byte order mark, surrounding blank lines, non-ASCII text.

### Editing by hand

The Mac and desktop config file is still a file, and editing it in an editor still works. A saved
change whose `controller` block you did not touch is treated as an ordinary edit: the app keeps a
copy of the previous bytes, rewrites only the `controller` block to bump the revision and extend the
lineage, and publishes on the next poll. Change the `id` by hand and you have declared a new setup,
which the machines will ask you about.

A document with no `controller` block at all — a hand-written file, or the pasted example — gets a
fresh setup id, revision 1 and an empty lineage before anything is applied or published, and the app
tells you the id it assigned.

### Bootstrapping a new device

**This device → Configuration → Fetch from a machine** takes a host, a port and a user, runs the
config command there with the device's own key (which has to be authorised on that system already,
exactly as for any other command), and applies what comes back. It tries the standard install
layouts in turn, so you do not have to know where the agent lives:

    /usr/bin/node /home/<user>/.legion-control/bin/launcher.mjs
    node C:\Users\<user>\.legion-control\bin\launcher.mjs
    node  |  /opt/homebrew/bin/node  with  ~/.legion-control/bin/launcher.mjs

Recognized older installs may still use `<base>/agent/src/index.mjs`. An existing explicit argv is
preserved. Moving it to the stable launcher is a reviewed configuration edit that keeps its
interpreter and base; custom paths are not silently rewritten or used to guess an upload location.

From then on the device reconciles like every other peer. Pasting the JSON by hand still works.

There is no discovery. Nothing on your network announces itself and no client listens for anything;
the document lists your machines and any one of them will hand it over. The document also carries no
public keys and nothing an agent would put into `authorized_keys` — authorising a device stays a
thing you do deliberately, on the machine, out of band. A setup file that could enrol keys would
mean one compromised phone could enrol keys everywhere.

## The agent contract

New installs use `node <base>/bin/launcher.mjs <command> [args] [--flags]`. Every invocation
prints exactly one JSON object to stdout and nothing else, including on failure. Progress and
diagnostics go to stderr and to `<base>/legionctl.log`. Exit code 0 when the command did what was
asked, 1 otherwise — and a deferral, a queued request or a conflict counts as having done what was
asked.

The machine-readable form of everything below is in `contract/`: the schemas, and a fixture for
every reply variant. Each client has a test that decodes every fixture, which is what keeps four
implementations honest about one contract.

### Commands

| Command | Args and flags | Changes anything | What it does |
| --- | --- | --- | --- |
| `status` | `--budget-ms N` (default 20000) | no | bounded snapshot: system, services, busy, operations, policy, boot targets, actions |
| `busy` | `--budget-ms N` | no | the aggregate busy object plus per-service evidence |
| `update` | `--service ID`, `--force`, `--detach`, `--when-idle`, `--expires DUR` (default 4h, max 7d) | yes | update one service on request |
| `restart` | `--service ID`, `--force`, `--detach`, `--when-idle`, `--expires DUR` | yes | restart one service |
| `boot` | `TARGET`, `--force`, `--no-reboot`, `--when-idle`, `--expires DUR` | yes | arm the named system and reboot |
| `sleep` | `--force`, `--when-idle`, `--expires DUR` | yes | suspend this machine |
| `run` | `ACTION`, `--force`, `--detach`, `--when-idle`, `--expires DUR` | yes | run a configured action |
| `cycle` | `--dry-run` | yes | the scheduled maintenance cycle over every eligible service and any queued requests |
| `auto-update` | `on`/`off`, `pause DUR`, `resume`, `--service ID` | config | the policy switches |
| `policy` | `--service ID`; `policy set` reads a JSON patch on stdin | config | read or change the effective update policy |
| `op` | `ID`, `--wait SECONDS` | no | one operation record; `--wait` long-polls until it finishes |
| `cancel` | `ID` | operations | cancel a queued operation |
| `history` | `--limit N` (default 30, max 200), `--service ID`, `--kind K` | no | operation summaries, newest first |
| `logs` | `--lines N` (default 100, max 500), `--op ID` | no | tail of the agent log, or one operation's log |
| `doctor` | `--service ID`, `--deep` | no | preflight and diagnosis, each check with a fix |
| `bundle` | | no | a diagnostic bundle: doctor, status, history, recent log, redacted config |
| `config` | `set` (document on stdin, `--controller-id ID --revision N`, `--replace`), `meta` | controller copy | read or replace the stored controller document |
| `service-config` | `get`, `validate --stdin`, `set --stdin` | local config | administrator reads, previews and saves service configuration; restricted keys refused |
| `self-update` | `--from PATH`, `--stdin`, `--install`, `--rollback`, `--check` | agent tree | replace the agent from a signed bundle, keeping the previous tree |
| `dispatch` | | as dispatched | the restricted-key entry point; see `security.md` |
| `version`, `help` | | no | |

`DUR` is a count and a unit: `30m`, `4h`, `2d`. Lifecycle commands (`update`, `restart`, `boot`,
`sleep`, `run`, `cycle`, `self-update`) take `--op ID`, a client-supplied identifier that binds the
request: asking again with the same id
reports on the original operation instead of starting a second one, and asking with the same id
for a *different* thing is a conflict rather than a replay.

To bootstrap a recognized older install, the client first verifies its bundled archive locally,
uploads it under `<base>/incoming`, then runs the new signed
`<base>/incoming/agent/src/index.mjs self-update --install --op ID`. The agent derives that base
from its staged path and rejects a conflicting `LEGIONCTL_HOME`. This mode does not accept
`--from` or `--stdin`. A v3 update uses one of those bundle sources directly.

The new stable launcher is installed under `<base>/bin` before the live tree moves. If the
process dies during a rename gap, it can restore the previous live tree using the swap journal
and its verified signature or anchored local pre-swap inventory. A local inventory is not a
publisher signature and does not enable ordinary unsigned rollback. A completed update retains
the previous tree; `self-update --rollback` activates it only after full signature, hash and
self-test checks. An unsigned 2.x backup remains available for independently verified manual
recovery. Existing direct `agent/src/index.mjs` paths work while that tree exists, but need the
stable launcher to recover a missing-tree gap.

`policy set` takes a JSON patch on stdin — `{ "automatic": …, "pauseUntil": …,
"maintenanceWindows": … }` — where a key left out is left alone, and on a service `null` means
inherit. Reading the machine's policy also returns the effective policy for every service, so a
client does not need one round trip per service to draw the maintenance rows.

Every reply carries the contract version and the agent version, so a client can tell what it is
talking to before it decides which features to offer. Keys are only ever added within a contract
version, never renamed or retyped, and every client decodes with every field optional.

Where a command does not do the obvious thing, the reply says why with a **reason code** as well as
a sentence. Clients render the code and fall back to the sentence, and treat a code they do not
know as "it did not happen, show the message". The codes cover the busy gate, the policy gate, the
operation lock, an unknown target or service, an invalid config, an unverifiable result, an
expired or cancelled request, and a bad argument.

`config set` takes the document on stdin and the setup id and revision as flags, which must match
the `controller` block inside the document — a mismatch is a bad argument and nothing is written.
It answers with what it did: stored, a no-op because those exact bytes were already there, or
replaced because a person chose to. A refusal says whether the pusher is behind or whether the two
copies have genuinely diverged, and carries the metadata of what the agent holds so the client can
work out which. `--replace` is the explicit human decision and is never sent automatically.

The document and its metadata are written together, in one transaction: there is no moment at which
a machine holds a document with somebody else's revision number beside it.

`operations.md` covers what an operation record contains, how a detached operation is polled, how
an interrupted one is recovered, and what `doctor` checks.
