# Configuration

Legion Control is three pieces that agree on one contract: an agent that runs on every machine
you want to control, a Mac menu bar app, and an Android app. None of them knows anything about
your machines until you tell them. This page is the whole of what you can tell them.

There are two configuration files, and they describe the same setup from two sides:

| File | Lives on | Describes |
| --- | --- | --- |
| **agent config** `~/.legion-control/config.json` | every controlled machine, one per system | what runs *here*: which system this is, which services to look after, how to switch boot targets, how to sleep, which extra actions to offer |
| **controller config** `~/.config/legion-control/config.json` | the Mac (the Android app takes the same JSON pasted into its settings) | which machines exist, how to reach them, how to wake them, and which systems each one can boot into |

Both are plain JSON. Both are validated on load, and a value that does not make sense falls back
to its default rather than propagating. Neither is ever written by an installer.

## Vocabulary

- A **machine** is a physical box: one network card, one hardware address, one thing to wake.
- A **system** is one operating system installed on a machine. A dual boot machine has two
  systems; only one of them is up at a time. Every system runs its own copy of the agent with its
  own config. A single boot machine has exactly one system.
- A **service** is something on a system that has a version, can be running or not, can be busy,
  and can be updated and restarted. The reference example is T3 Code, but a service can be any
  npm package, any self updating desktop app, or anything you can describe with a few commands.
- An **action** is a named command you want a button for. No version, no health, no busy state
  of its own: just something to run.
- The **local system** is the device the controller app itself runs on. The Mac app can run the
  agent locally, without ssh, to look after services on the Mac itself.

## The agent config

`~/.legion-control/config.json` (`C:\Users\<you>\.legion-control\config.json` on Windows). The
install base can be moved with `LEGIONCTL_HOME`. Every key is optional; the file itself is
optional.

```jsonc
{
  // Which system this is. The id is what the controller apps match against, and what the other
  // system on a dual boot machine names as a boot target. Defaults to the platform name
  // ("linux", "windows" or "mac") and "Linux" / "Windows" / "macOS".
  "system": { "id": "cachyos", "name": "CachyOS" },

  // Whether the scheduled update cycle is allowed to act. "update" without --force is a no-op
  // while this is off. Global for the system, not per service.
  "autoUpdate": true,

  // The services to look after. See "Service kinds" below.
  "services": [ /* ... */ ],

  // How to boot into another system. Keys are the target system ids. See "Boot targets".
  "boot": { "targets": { /* ... */ } },

  // How to suspend this machine. See "Sleep".
  "sleep": { /* ... */ },

  // Extra buttons. See "Actions".
  "actions": [ /* ... */ ]
}
```

### Service kinds

Every service has an `id` (short, stable, used on the command line as `--service <id>`), a
`name` (what the apps show), a `kind`, and three optional probes: `process`, `health` and `busy`.
The kind decides where versions come from and how an update happens.

#### `npm`: a package installed globally with npm

```jsonc
{
  "id": "t3",
  "name": "T3 Code",
  "kind": "npm",
  "package": "t3",
  // The dist-tag to follow. The update installs whatever the tag points at.
  "channel": "nightly",
  // Anything the tag resolves to must match this, or the update refuses. Guards against a
  // malformed dist-tag being handed to npm install. Default accepts any semver-shaped string.
  "versionPattern": "^\\d+\\.\\d+\\.\\d+-nightly\\.\\d+\\.\\d+$",
  // The global prefix the package lives under. Default: the platform's usual one, probed if
  // that is empty.
  "npmPrefix": null,
  // Packages allowed to run install scripts on npm 12 and newer.
  "allowScripts": ["node-pty", "msgpackr-extract"],
  "process": { "type": "systemd-user", "unit": "t3-code.service" },
  "health": { "type": "http", "port": 3773, "path": "/" },
  "busy": { "type": "t3-sqlite" }
}
```

The update cycle for this kind: resolve the tag, compare with the installed `package.json`
version, check busy, pre-warm the npm cache, take the maintenance lock, stop the process,
`npm install --global`, verify the installed version, start, wait for health. Any failure
reinstalls the previous version. The agent never installs while the service is busy.

#### `app`: a desktop app that updates itself

For apps that download their own updates and apply them when they quit (electron-updater and
similar). The agent never downloads anything; it notices that a build is staged and, when nothing
is running, asks the app to quit and starts it again. macOS only today.

```jsonc
{
  "id": "t3",
  "name": "T3 Code",
  "kind": "app",
  "path": "/Applications/T3 Code (Nightly).app",
  "bundleId": "com.t3tools.t3code",
  // Where the app stages a downloaded build: <updaterCacheDir>/pending/update-info.json
  "updaterCacheDir": "~/Library/Caches/t3code-updater",
  // How to read the version out of the staged file name. The first capture group is the version.
  "stagedFilePattern": "^T3-Code-(.+)-(?:arm64|x64|universal)\\.(?:zip|dmg)$",
  // Where the newest version is announced, for reporting only. Nothing is installed from it.
  "latest": { "type": "github-releases", "repo": "pingdotgg/t3code", "prerelease": true },
  "health": { "type": "http", "port": 3773, "path": "/" },
  "busy": { "type": "t3-sqlite" }
}
```

`process` is implied for this kind: running means a process whose executable sits inside the
bundle exists, stop means `osascript quit app id`, start means `open -a`. The quit is asked for
and never forced.

#### `command`: anything you can describe with commands

```jsonc
{
  "id": "sunshine",
  "name": "Sunshine",
  "kind": "command",
  // Each of these is an argv array, never a shell string. Optional ones may be left out.
  "installedVersion": ["sunshine", "--version"],   // stdout, first line, trimmed
  "latestVersion": ["sh", "-c", "curl -s https://.../latest | jq -r .tag_name"],
  "update": ["paru", "-S", "--noconfirm", "sunshine"],
  "process": { "type": "systemd-user", "unit": "sunshine.service" },
  "health": { "type": "http", "port": 47990, "path": "/" },
  "busy": { "type": "none" }
}
```

Without `latestVersion` the service reports `upToDate: null` and the apps say the version was
not checked; the scheduled cycle then leaves it alone, since running an unconditional `update`
every 15 minutes would stop and start the service each quarter hour for nothing, and only
`update --force` runs it. Without `update` the service cannot be updated from the apps, only
restarted. There is no rollback for this kind, because there is nothing to put back; the cycle
only guarantees the service comes back up.

### Probes

**`process`**, how the service is started, stopped and looked at:

| type | fields | notes |
| --- | --- | --- |
| `systemd-user` | `unit` | Linux. `systemctl --user start/stop/is-active` |
| `systemd-system` | `unit` | Linux. `sudo -n systemctl start/stop/is-active` |
| `scheduled-task` | `task` | Windows. `Start-/Stop-ScheduledTask`, running is decided by a process whose command line contains `match` (default: the npm package root) |
| `app` | | macOS, implied for kind `app` |
| `command` | `start`, `stop`, `running` | argv arrays. `running` is judged by exit code 0 |
| `none` | | nothing to start or stop, so nothing that can be down: reported as running, and not restartable |

**`health`**, whether the service is answering:

| type | fields |
| --- | --- |
| `http` | `port`, `path` (default `/`), `host` (default `127.0.0.1`). Healthy means HTTP 200 |
| `command` | `command`, exit code 0 means healthy |
| `none` | healthy whenever the process is running |

**`busy`**, whether something would be lost by stopping the service. This gate sits in front
of every disruptive action (update, restart, boot, sleep) and it fails closed: a probe that cannot
be read counts as busy.

| type | fields | notes |
| --- | --- | --- |
| `t3-sqlite` | `home` (default `~/.t3` or `$T3CODE_HOME`), `staleHours` (default 6) | counts running and pending turns and pending approvals in T3 Code's own state database |
| `command` | `command`, `staleHours` | exit code 0 means idle, anything else means busy. If stdout is a JSON object with `busy` and `reason`, those are used |
| `http` | `port`, `path`, `host`, `busyWhen` | GET the URL; busy when the JSON body's `busyWhen` path (dotted) is truthy |
| `none` | | never busy |

A `relay` block on a service names a tunnel process that has to be up for the service to count
as reachable from outside: `{ "type": "cloudflared" }` today. Leave it out when there is none.

`lockFile` on a service names the maintenance lock the update takes while it works, for the
case where something else on the machine (a watchdog task, say) already honours a particular
file. Default: `<base>/update.lock`.

### Boot targets

`boot.targets` is keyed by the id of the system to boot into. Each entry says how to arm the
firmware from *this* system. Arming is always read back and verified before anything reboots;
a reboot that was not verifiably armed is refused, because rebooting an unarmed machine just
returns to the same system and the apps would sit waiting for one that never comes.

| method | platform | fields | what it does |
| --- | --- | --- | --- |
| `efi-bootnext` | Linux | `match` (regex against the entry description) | `efibootmgr --bootnext <entry>`, entry resolved by description on every call, never cached |
| `clear-bootsequence` | Windows | | `bcdedit /deletevalue {fwbootmgr} bootsequence`, so the next boot falls through BootOrder |
| `bootsequence` | Windows | `entry` (a `{guid}` or `{bootmgr}`) | `bcdedit /set {fwbootmgr} bootsequence <entry>` |
| `grub-reboot` | Linux | `entry` | `sudo -n grub-reboot <entry>`, read back from `grub-editenv list` |
| `command` | any | `arm` (argv), `verify` (argv, optional, exit code 0 means armed) | whatever you say. Without `verify` this is the one arm path that is not read back, and the reply says so |

`boot.reboot` overrides the reboot command (argv). Default: `shutdown /r /t 3` on Windows,
`sudo -n sh -c 'sleep 3; systemctl reboot'` on Linux. A `name` on a target is optional and is
only used when the controller has no name for that id.

```jsonc
"boot": {
  "targets": {
    "windows": { "method": "efi-bootnext", "match": "^Windows Boot Manager\\b" }
  }
}
```

and on the Windows side of the same machine:

```jsonc
"boot": {
  "targets": {
    "cachyos": { "method": "clear-bootsequence" }
  }
}
```

### Sleep

```jsonc
"sleep": {
  // Commands to run first, each an argv array. For anything on the machine that holds sleep
  // off while an ssh session is open.
  "before": [["powershell.exe", "-NoProfile", "-Command", "Stop-ScheduledTask -TaskName MyGatekeeper"]],
  // Seconds to wait after the "before" commands, for whatever they stopped to actually let go.
  // Only waited when there was something to run first.
  "settle": 2,
  // The suspend command itself. Defaults: `sudo -n systemctl suspend` on Linux, `pmset sleepnow`
  // on macOS, and on Windows the first of `tools` that exists, run as `<tool> -d -t 3 -accepteula`.
  "command": null,
  // Windows only: psshutdown candidates, best first. The built in rundll32 and .NET suspend
  // calls are silently vetoed on some machines, which is why this is a list of tools.
  "tools": ["C:\\Tools\\psshutdown64.exe"]
}
```

The agent only ever learns that the suspend command was accepted; the machine goes down
underneath it. `action: "sleeping"` means accepted, not confirmed.

### Actions

```jsonc
"actions": [
  {
    "id": "sunshine-restart",
    "name": "Restart Sunshine",
    "command": ["systemctl", "--user", "restart", "sunshine"],
    // Shown before running. Leave it out for actions that need no confirmation.
    "confirm": "The streaming host restarts and any open stream drops.",
    // Refuse while any service is busy, unless forced.
    "busyGated": false,
    "timeoutSeconds": 60
  }
]
```

Actions appear in `status` under `actions` and run with `legionctl run <id> [--force]`. The
reply carries `action: "ran" | "deferred" | "failed"`, the exit code, and the first lines of
output.

### Legacy keys

A config written for the first version of the agent (`port`, `channel`, `allowScripts`,
`npmPrefix`, `staleTurnHours`, `t3AppPath`, `t3AppBundleId`, `updaterCacheDir`) still works:
when `services` is absent, those keys describe one T3 Code service exactly as before, with the
platform's default process type (`t3-code.service`, the `T3 Code Connect` task, or the app
bundle), the `t3-sqlite` busy probe, the strict nightly `versionPattern` the first version
enforced, and on Windows the `%LOCALAPPDATA%\T3Code\update.lock` lock file. A new-style
config gets none of those defaults: name the unit, the task and the lock file yourself. With no `boot.targets`, a Linux system offers `windows`
through `efi-bootnext` matching `^Windows Boot Manager\b`, and a Windows system offers `linux`
through `clear-bootsequence`. With no `system`, the id is the platform name. So a machine that
was set up before `services` existed keeps working, and its controller can keep naming the two
systems `linux` and `windows`.

## The controller config

`~/.config/legion-control/config.json` on the Mac (`LEGION_CONTROL_CONFIG` overrides the path,
for trying a second config without touching the real one). The Android app takes the same
document pasted into **This device → Configuration**. The Mac app watches the file through its
directory, so an editor's write-and-rename is picked up and a save lands in the window with no
restart. A config that does not parse or validate never replaces one that did: the machines
stay and the reason appears on the setup page and under "Legion Control" in This Mac. On the
phone, Apply stores nothing until the document passes.

Validation is strict where a fallback would be a guess: ids must be unique, every machine
needs at least one system and either `ssh` or an endpoint, every system needs a non-empty
`agent` argv, and a `wake` block needs a parseable `mac`. Missing names fall back to ids,
missing ports to 22, an unknown `platform` to `linux`.

```jsonc
{
  "version": 1,
  "machines": [
    {
      "id": "legion",
      "name": "Legion",

      // Mac: what to hand to /usr/bin/ssh. An alias from ~/.ssh/config is the simplest thing,
      // and lets ssh do the routing. Optional user, port and identityFile are passed through.
      "ssh": { "host": "legion" },

      // Android, and the Mac when "ssh" is absent: addresses to dial directly, in preference
      // order. "kind" is "lan" (broadcast domain shared with the machine, wake works) or
      // "remote" (a tailnet or VPN address; no wake). "system" is a routing hint: an address
      // that belongs to one system saves a wasted round trip guessing the other one.
      "endpoints": [
        { "id": "tailnet-linux", "kind": "remote", "host": "100.64.0.10", "port": 22, "user": "me", "system": "cachyos", "label": "Tailnet, CachyOS" },
        { "id": "tailnet-windows", "kind": "remote", "host": "100.64.0.11", "user": "me", "system": "windows", "label": "Tailnet, Windows" },
        { "id": "lan", "kind": "lan", "host": "192.168.1.40", "user": "me", "label": "Home LAN" }
      ],

      // Wake on LAN. Leave the block out for a machine that cannot be woken.
      "wake": {
        "mac": "AA:BB:CC:DD:EE:FF",
        "broadcast": ["192.168.1.255"],
        "ports": [9, 7],
        // Polled on TCP after the packet, to tell when the machine is back.
        "probe": { "host": "192.168.1.40", "port": 22 },
        // Android only: the phone counts as at home when one of its addresses starts with this.
        "lanPrefix": "192.168.1."
      },

      // Every system installed on the machine. "id" must match the agent's system.id on that
      // system. "agent" is the exact argv run over ssh; it must not need quoting.
      "systems": [
        { "id": "cachyos", "name": "CachyOS", "platform": "linux",
          "agent": ["/usr/bin/node", "/home/me/.legion-control/agent/src/index.mjs"] },
        { "id": "windows", "name": "Windows 11", "platform": "windows",
          "agent": ["node", "C:\\Users\\me\\.legion-control\\agent\\src\\index.mjs"] }
      ]
    }
  ],

  // The device the app runs on, looked after by a local agent. Mac only.
  "local": { "enabled": true, "name": "This Mac", "agent": "~/.legion-control/agent/src/index.mjs" },

  // Where both apps look for their own updates. Default: this project's releases.
  "appUpdates": { "githubRepo": "x1f4r/legion-control" }
}
```

`platform` is `linux`, `windows` or `mac` and picks the icon and the wording. A `symbol` on a
system overrides the icon on the Mac with any SF Symbol name.

`appUpdates.githubRepo` is read by both apps now, not just the phone, and both read the same
releases: tag `vX.Y.Z`, the version taken off the tag, and one asset per platform, the first
ending in `.zip` for the Mac and the first ending in `.apk` for Android. The Mac app looks when
the window or the menu bar panel opens and at most once every six hours; Install downloads the
zip, checks the bundle in it is this app at that version, swaps `/Applications/Legion Control.app`
and relaunches. The key is optional on both sides and defaults to this project's own releases; a
fork names itself here instead. `scripts/release.sh` is what produces releases in that shape.

With no config file the Mac app opens its window on a setup page showing the path, with
**Write example config** (refuses to overwrite an existing file) and **Open in editor**; the
menu bar panel says "No machines configured" until then. The Android app ships the same example
behind **Insert example**; its hardware address is a placeholder that Apply refuses until it is
replaced.

Against an agent that reports no `bootTargets` key at all (an older agent), the apps offer
every other configured system as a boot target; an agent that reports an empty list offers
none. The apps send `--service` only to an agent that reports `services`, since an older one
rejects a flag it does not know.

### Reading an older agent

The apps accept an agent that predates `services`: a status without `services` is read as one
service built from its `t3` block, and a status without `system` takes the platform name as the
system id. That is what lets the apps be updated before the machines are.

## Sharing the setup between devices

Writing the controller config once on the Mac and once on every phone is one time too many,
so the machines carry it. Every agent keeps a copy of the controller config at
`<base>/controller.json`, opaque to it: the agent never reads the document, it only stores and
serves the bytes it was given.

- `legionctl config` prints `{ "ok": true, "controller": <the document or null>, "hash": "<sha256 of its bytes or null>" }`.
- `legionctl config set` reads a JSON document from stdin (up to 1 MB), checks it parses as an
  object with a `version` and a `machines` array, writes it atomically, and prints the new hash.
- `status` carries `controller: { "hash": ... }` (null when nothing is stored).

The Mac is the source of truth. Whenever a machine reports a `controller.hash` that differs from
the sha256 of the Mac's own config file, the Mac app pushes the file with `config set`, to the
machine and to its own local agent. Editing the file on the Mac therefore reaches every machine
the next time it is up, with nothing to do by hand.

The phone bootstraps from any one machine: **This device → Configuration → Fetch from a
machine** takes a host (a tailnet or LAN address), a port and a user, runs `config` there with
the phone's own key (which has to be authorised on that system already, exactly as for any
other command), and applies what comes back. It tries the standard install layouts in turn:
`/usr/bin/node /home/<user>/.legion-control/agent/src/index.mjs`,
`node C:\Users\<user>\.legion-control\agent\src\index.mjs`, and `node` / `/opt/homebrew/bin/node`
with `~/.legion-control/agent/src/index.mjs`. From then on, whenever a status reply reports a
`controller.hash` different from the hash of the document the phone applied, the phone fetches
the document again from that machine and applies it, so an edit on the Mac reaches the phone
by itself. Pasting the JSON by hand still works and is never overwritten by an older copy: a
document that fails validation is refused, and the phone remembers the hash of what it applied.

An agent that predates `config` (any 1.x) has nothing to serve; the apps say so and fall back
to the manual paste.

## The agent contract

`legionctl` is `node <base>/agent/src/index.mjs <command> [flags]`. Every invocation prints
exactly one JSON object to stdout and nothing else, including on failure. Progress and
diagnostics go to stderr and to `<base>/legionctl.log`. Exit code 0 when the command did what
was asked, 1 otherwise.

| Command | Flags | What it does |
| --- | --- | --- |
| `status` | | read-only snapshot: system, services, busy, boot targets, actions |
| `busy` | | the busy object: with one service its full detail, with several the aggregate plus a `services` array |
| `update` | `--force`, `--service <id>` | one update cycle for one service (default: the first) |
| `restart` | `--force`, `--service <id>` | restart one service; refuses while busy unless forced |
| `auto-update` | `on` or `off` | persists `autoUpdate` |
| `boot <target>` | `--force`, `--no-reboot` | arm the named target and reboot; refuses while busy unless forced |
| `sleep` | `--force` | suspend this machine; refuses while busy unless forced |
| `run <action>` | `--force` | run a configured action; refuses while busy if it is busy gated and not forced |
| `version` | | the agent version |
| `help` | | the command list |

`status` answers:

```jsonc
{
  "ok": true,
  "os": "linux",
  "system": { "id": "cachyos", "name": "CachyOS" },
  "hostname": "legion",
  "agentVersion": "2.0.0",
  "services": [
    {
      "id": "t3", "name": "T3 Code", "kind": "npm",
      "installed": "0.0.36-nightly.20260827.1206",
      "latest": "0.0.36-nightly.20260827.1206",   // null when it could not be checked
      "channel": "nightly",
      "upToDate": true,                            // null when latest is null
      "running": true, "healthy": true, "port": 3773,
      "staged": null, "appPath": null,             // kind "app" only
      "busy": { "busy": false, "reason": "idle", "unknown": false, /* ... */ },
      "relay": { "configured": false, "running": false },
      "pendingRestart": false,
      "lastUpdate": { "at": "...", "from": "...", "to": "...", "result": "ok", "message": "..." },
      "canUpdate": true, "canRestart": true
    }
  ],
  "busy": { "busy": false, "reason": "idle", "unknown": false },   // any service busy
  "bootTargets": [ { "id": "windows", "name": null } ],
  "actions": [ { "id": "sunshine-restart", "name": "Restart Sunshine", "confirm": "...", "busyGated": false } ],
  "autoUpdate": true,
  "notes": [],
  // Kept for one release so older apps keep reading the first service. Do not build on it.
  "t3": { "installed": "...", "nightly": "...", "upToDate": true, "serverRunning": true, "healthy": true, "port": 3773 },
  "pendingRestart": false, "lastUpdate": { /* first service */ }, "connect": { /* first service's relay */ }
}
```

`update`, `restart`, `boot`, `sleep` and `run` answer with `ok`, `action` and `message`, plus
`from` and `to` for an update, `target` for a boot, and `service` for anything that acted on
one. `action` is one of:

| Command | `action` |
| --- | --- |
| update | `updated`, `noop`, `deferred`, `failed`, `rolled-back` |
| restart | `restarted`, `deferred`, `failed` |
| boot | `rebooting`, `armed`, `noop`, `deferred`, `failed` |
| sleep | `sleeping`, `deferred`, `failed` |
| run | `ran`, `deferred`, `failed` |

`deferred` always means the busy gate held the action back and `--force` would override it.
The apps turn that into a question rather than a retry.

`auto-update on|off` rewrites only the `autoUpdate` key of `config.json` and leaves every other
key exactly as it found it.

State is per service: `state.json` keeps `lastUpdate`, `pendingRestart` and the version cache
under `services.<id>`. A state file written by the first version of the agent is read as the
first service's, so an in-place upgrade forgets nothing.
