# Guides

Task-oriented walk-throughs for day-to-day work with Legion. Every transcript below comes from the
built binary with placeholder machine names, users, and addresses.

## Files and transfer (`legion cp` and `fs.transfer`)

`legion cp` is the command-line wrapper over `fs.transfer`. Each endpoint is either a local path
(absolute or starting with `~`, including Windows drive paths like `C:\Users\me\file.txt`) or
`machine:/path` for a machine in `fleet.toml`. `~` always expands on the machine that endpoint
names.

### "The destination is the path itself"

`to` names the exact destination path:

- Copying a **directory** `~/src` to `pi:~/dst` syncs the contents of `~/src` into `pi:~/dst` (not
  `pi:~/dst/src`).
- Copying a **single file** `~/docs/hello.txt` to `pi:~/docs/copied.txt` writes
  `pi:~/docs/copied.txt`. The one exception is copying a single file to an existing directory (or
  to a path ending in `/`), which places the file inside that directory under its own filename.

### Syncing a directory tree

Preview first with `--dry-run`, then sync:

```sh
legion cp ~/src pi:~/dst --dry-run
```

```text
from             tower:/home/me/src
to               pi:~/dst
kind             tree
files_sent       2
bytes_sent       11
files_unchanged  0
deleted          0
conflicts        -
skipped          -
duration_ms      5
dry_run          true
would_send       a.txt, sub, sub/b.txt
would_delete     -
```

```sh
legion cp ~/src pi:~/dst
```

```text
from             tower:/home/me/src
to               pi:~/dst
kind             tree
files_sent       2
bytes_sent       11
files_unchanged  0
deleted          0
conflicts        -
skipped          -
duration_ms      16
dry_run          false
```

Running the same command again compares size and modification time and sends nothing:

```sh
legion cp ~/src pi:~/dst
```

```text
from             tower:/home/me/src
to               pi:~/dst
kind             tree
files_sent       0
bytes_sent       0
files_unchanged  2
deleted          0
conflicts        -
skipped          -
duration_ms      5
dry_run          false
```

### Deleting extras, checksums, exclusions, and resume

- `--delete` removes files and directories at the destination that no longer exist at the source.
  Anything matched by `--exclude` is ignored at both ends and never deleted.
- `--checksum` compares files by BLAKE3 hash instead of size and modification time (reads every
  file at both ends).
- `--exclude <glob>` is repeatable; a trailing `/` matches directories only (for example
  `--exclude target/ --exclude '*.tmp'`).
- **Single-file resume**: when a large single-file transfer is interrupted, the destination keeps
  its partial file beside the target. Re-running `legion cp` asks the destination where it stopped,
  streams only the remaining bytes, verifies the whole-file BLAKE3 digest, and atomically renames
  the file into place.

```sh
legion cp ~/src pi:~/dst --delete --checksum
```

```text
from             tower:/home/me/src
to               pi:~/dst
kind             tree
files_sent       0
bytes_sent       0
files_unchanged  2
deleted          1
conflicts        -
skipped          -
duration_ms      7
dry_run          false
```

Progress is rendered on one line of `stderr` when `stderr` is a terminal. Bytes always move over a
dedicated `legion stream` pipe (never inside JSON), relayed through the calling node, and every
operation is checked against the target machine's `policy.toml` (`fs.read`, `fs.write`, or
`fs.remove`) and written to its audit log with door `stream`.

## Long jobs (`job.*`)

Use `job.*` for anything that may outlive your terminal or SSH connection. `job.start` writes the
job record under `state/jobs/<id>/` before starting a detached supervisor process (a transient
`systemd-run --user --scope` on Linux when a user manager is running, `setsid` otherwise, or a
process outside the caller's job object on Windows — Windows not run here).

### Start with an idempotency `key`

Pass `key` so retrying after a dropped network link returns the existing job (`existing: true`)
instead of starting a second copy:

```sh
legion job.start cmd='echo step-1; sleep 0.3; echo step-2' key=demo-build --json
```

```json
{"id":"0mwu38930-ynsu5a","name":"echo","state":"running","created":"2026-11-24T02:40:54.012Z","existing":false,"warnings":[]}
```

Calling `job.start` again with the same `key` and identical command, `cwd`, and environment variable
names returns the original job (`existing: true`); changing the arguments with the same `key` fails
with `conflict`.

Set `exclusive=true` when the job should mark the machine busy and block unattended `power.*`
calls.

### Wait for completion and follow logs

Use `job.wait` instead of tight-polling `job.status`:

```sh
legion job.wait id=0mwu38930-ynsu5a timeout_s=10 --json
```

```json
{
  "id": "0mwu38930-ynsu5a",
  "name": "echo",
  "key": "demo-build",
  "cmd": "echo step-1; sleep 0.3; echo step-2",
  "env_names": [],
  "principal": "owner",
  "created": "2026-11-24T02:40:54.012Z",
  "boot_id": "00000000-0000-4000-8000-000000000001",
  "exclusive": false,
  "state": "succeeded",
  "exit_code": 0,
  "finished": "2026-11-24T02:40:54.349Z",
  "duration_s": 0.337,
  "output_bytes": 14,
  "timed_out": false
}
```

Stream output incrementally with `job.logs` by passing `offset=0` and feeding each reply's
`next_offset` back into the next call (or omit `offset` to read the last `tail` lines, default
100):

```sh
legion job.logs id=0mwu38930-ynsu5a offset=0 --json
```

```json
{
  "id": "0mwu38930-ynsu5a",
  "state": "succeeded",
  "content": "step-1\nstep-2\n",
  "offset": 0,
  "next_offset": 14,
  "eof": true,
  "truncated": false,
  "output_dropped": false
}
```

### Cancel a running job

`job.cancel` terminates the job and its entire process tree, escalating after a short grace period
(or immediately with `force=true`), and reports `cancelled` once the processes are gone:

```sh
legion job.cancel id=0mwu389f8-j1jyni --json
```

```json
{
  "id": "0mwu389f8-j1jyni",
  "state": "cancelled",
  "survivors": []
}
```

### What `interrupted` means

Every job record stores the supervisor's process id, its OS process start time, and the machine's
`boot_id`, and the supervisor holds a kernel lock on `state/jobs/<id>/lock` while running. If the
machine loses power or the supervisor dies without writing `exit.json`, `job.status` and `job.list`
derive the state `interrupted` without modifying anything on disk. A recycled PID never makes a
dead job look alive.

## Projects (`proj.*`)

`projects.toml` maps named projects to their directory on each machine (`[projects.<name>.on]`),
optional `repo` and `description`, and named tasks (`[projects.<name>.tasks]`):

```toml
[projects.site]
description = "Public website"
repo = "git@git.example:me/site.git"

[projects.site.on]
tower = "~/projects/site"
pi = "~/projects/site"

[projects.site.tasks]
check = "ls -1"
dev = { cmd = "npm run dev", job = true }
build = { argv = ["cargo", "build", "--release"], timeout_s = 1800 }
```

Because `projects.toml` stores paths keyed by node name, the same file can be synced across every
machine with `fleet.sync`.

List registered projects on this node, inspect git status across every machine at once with
`--on all`, and run a named task:

```sh
legion proj.list
```

```text
node  tower
projects
  - name         site
    description  Public website
    repo         git@git.example:me/site.git
    path         ~/projects/site
    present      true
    elsewhere    pi
    tasks        check
```

```sh
legion proj.status project=site --on all
```

```text
pi
  projects
    - name     site
      path     /home/me/projects/site
      present  true
      git
        branch    main
        detached  false
        head      2a04486af317e0503b114618724370abd6fce599
        dirty
          staged      0
          unstaged    0
          untracked   0
          conflicted  0
        clean     true
        last_commit
          hash     2a04486af317e0503b114618724370abd6fce599
          subject  Initial commit
          author   me
          time     2026-11-24T02:41:11Z
        stash     0

tower
  projects
    - name     site
      path     /home/me/projects/site
      present  true
      git
        branch    main
        detached  false
        head      2a04486af317e0503b114618724370abd6fce599
        dirty
          staged      0
          unstaged    0
          untracked   0
          conflicted  0
        clean     true
        last_commit
          hash     2a04486af317e0503b114618724370abd6fce599
          subject  Initial commit
          author   me
          time     2026-11-24T02:41:11Z
        stash     0
```

```sh
legion proj.run project=site task=check
```

```text
README.md
project           site
task              check
exit_code         0
stdout            README.md
stderr            
stdout_truncated  false
stderr_truncated  false
timed_out         false
duration_ms       0
```

Tasks with `job = true` start through `job.start` and return a job id instead of waiting inline.
Use `proj.discover` to find existing git checkouts under a directory, `proj.add` / `proj.remove` to
edit `projects.toml` safely, and `proj.clone` / `proj.pull` to fetch code.

## Services (`svc.*`)

`svc.*` drives the native service manager on each OS with exact argv (never a shell) and the `C`
locale:

- **Linux**: `systemd` (`systemctl` and `journalctl`), supporting `user` and `system` scopes.
- **macOS** *(not run here; from code)*: `launchd` (`launchctl`), user services in `gui/<uid>` and
  system daemons in `system`; `svc.logs` reads log paths declared in the service plist.
- **Windows** *(not run here; from code)*: Windows Service Control Manager (`system` scope only)
  and Windows Event Log.

Read-only tools (`svc.list`, `svc.status`, `svc.logs`) never escalate privileges:

```sh
legion svc.list scope=user limit=5
```

```text
backend    systemd
services
  - name         app-org.gnome.SettingsDaemon.DiskUtilityNotify@autostart.service
    scope        user
    state        stopped
    description  Disk Utility notification plugin
truncated  true
```

When `scope` is omitted on `svc.status`, `svc.logs`, `svc.start`, `svc.stop`, or `svc.restart`,
Legion checks `user` first and then `system`, failing with `conflict` if the same service name
exists in both scopes.

Control tools (`svc.start`, `svc.stop`, `svc.restart`) carry the `exec` effect and read the
service state back after a short settle window so a service that crashes immediately is not
reported as running. To allow controlling **system** services on Unix, opt in explicitly in
`node.toml` and configure narrow non-interactive `sudoers` rules:

```toml
[svc]
sudo = true
```

## Power (`power.*`)

Power tools never claim an action succeeded because the machine goes down underneath the call.
Instead, `power.sleep`, `power.restart`, and `power.shutdown` schedule the action a few seconds out
so the reply leaves first, and every disruptive action passes through the busy gate.

### Busy and `unmonitored`

A node reports one of three busy states in `power.status`:

- `unmonitored` — `node.toml` configures no `busy` setting. Unattended power calls fail with `busy`
  because not knowing is not the same as idle.
- `busy` — at least one `[[busy.probes]]` exited `0` (or failed/timed out — fail closed) or an
  `exclusive` job is running.
- `idle` — `busy = "none"` is declared explicitly, or every probe exited `1` and no exclusive job
  is running.

Configure `node.toml` with probes (or `busy = "none"` if nothing needs protecting) and optional
boot targets:

```toml
name = "tower"
system = "linux"
busy = "none"

[power]
sudo = true

[boot.windows]
efi_label = "Windows Boot Manager"
```

### Always check `power.status` and `dry_run=true` first

```sh
legion power.status
```

```text
system   linux
boot_id  00000000-0000-4000-8000-000000000001
busy
  state    idle
  reasons  -
targets
  - name  windows
    how   efibootmgr --bootnext, firmware entry labelled "Windows Boot Manager"
sudo     true
```

```sh
legion power.sleep dry_run=true
```

```text
action     sleep
scheduled  false
in_s       3
boot_id    00000000-0000-4000-8000-000000000001
commands
  - sudo, -n, systemctl, suspend
dry_run    true
```

### Restarting into another system (`power.restart` and `power.switch`)

`power.restart into=windows` resolves the firmware entry by label on every call (never cached),
arms one-time boot (`efibootmgr --bootnext` on Linux, `bcdedit` on Windows, or your custom `arm`
and `verify` commands), reads the setting back, and schedules the reboot only if the readback
matches:

```sh
legion power.restart into=windows dry_run=true
```

```text
action     restart
scheduled  false
in_s       3
into       windows
entry      Boot0000 "Windows Boot Manager"
boot_id    00000000-0000-4000-8000-000000000001
commands
  - sudo, -n, efibootmgr, --bootnext, 0000
  - sudo, -n, systemctl, reboot
dry_run    true
```

To restart **another** machine and confirm it actually came back in the expected system with a new
`boot_id`, call `power.switch` from a different machine:

```sh
legion power.switch machine=pi into=windows dry_run=true
```

```text
machine      pi
from_system  linux
restarted    false
waited_s     0.0
would_run
  action     restart
  scheduled  false
  in_s       3
  into       windows
  boot_id    00000000-0000-4000-8000-000000000001
  commands
    - true
    - systemctl, reboot
  dry_run    true
dry_run      true
```

### Waking through a helper (`power.wake` and `power.wol`)

Magic packets are local-network broadcasts and cannot cross a Tailscale tunnel. Set `wake_via` on
the machine you call from, and set `mac` in `fleet.toml` on the helper machine (`pi`) — `power.wol`
always reads the hardware address from the helper's own `fleet.toml`, never from the request. A
sleeping helper is skipped and never woken to act as a helper.

```sh
legion power.wake machine=shed dry_run=true
```

```text
machine   shed
sent_by   pi
packets   0
awake     false
waited_s  0.0
would_send
  machine       shed
  mac           02:00:00:00:00:03
  packets       0
  interfaces
    - name       eth0
      address    192.0.2.10
      broadcast  192.0.2.255
  destinations  192.0.2.255:9, 255.255.255.255:9
  dry_run       true
dry_run       true
```

Pass `wait_s=60` on a real wake call so `power.wake` polls until `shed` itself answers `hello` and
reports `awake: true`.

### Checklist: try once under supervision on real hardware

Before relying on unattended power or dual-boot switching, test each step once while you can walk
over to the machine:

1. Run `legion power.status` on each OS of the machine and verify `system`, `boot_id`, `busy`,
   `targets`, and `sudo`.
2. Run `dry_run=true` on `power.sleep`, `power.restart into=<target>`, `power.wake`, and
   `power.switch`, and read every command it plans to run.
3. Verify non-interactive privileges: on Unix, run `sudo -n -l` for the exact sleep/reboot/efibootmgr
   commands; on Windows, verify `psshutdown` is on `PATH` (or configure `[power] sleep`) and that
   your SSH session can run `bcdedit`.
4. Run one supervised `power.sleep` and confirm the machine suspends and resumes cleanly (including
   any `[power] before_sleep` hooks).
5. Run one supervised `power.wake machine=<name> wait_s=90` from another machine via its wake
   helper to verify BIOS/NIC Wake-on-LAN and broadcast reachability.
6. Run one supervised `power.switch machine=<name> into=<other>` to confirm one-time boot lands in
   the requested system, then reboot plainly and confirm the firmware returns to the default OS.

## The HTTP door (`legion serve`)

`legion serve` exposes the node's tools over HTTP for phones, scripts, and remote MCP clients. It
listens on loopback only (`127.0.0.1:7117` by default) and refuses any non-loopback `--listen`
address.

### Create a token

First make sure the principal exists in `policy.toml`, then create a bearer token:

```sh
legion token add phone phone
```

```json
{"name":"phone","principal":"phone","token":"lgn_placeholder_copy_from_your_own_output"}
```

Copy the token from that reply immediately: `tokens.toml` stores only its BLAKE3 hash, so the
token cannot be shown again. Pass `expires_days=90` for an expiring token, `legion token list` to
inspect tokens, and `legion token remove phone` to revoke one (revoking also terminates any active
MCP sessions opened with that token). Creating a token for `owner` requires `allow_owner=true`.

### Start the server and publish with Tailscale Serve

```sh
legion serve
```

To reach it from your phone or another device on your tailnet over HTTPS, publish the loopback port
with Tailscale Serve:

```sh
tailscale serve --bg --https=8443 http://127.0.0.1:7117
```

Add the Tailscale host name (and any browser origins, if a web page will call it) to `node.toml`:

```toml
[serve]
hosts = ["tower.example.ts.net"]
origins = ["https://app.example.net"]
```

Requests with any unlisted `Host` are refused (stopping DNS rebinding), and requests with an
unlisted `Origin` are refused with `403` (stopping random browser tabs from calling the loopback
port).

### Calling the HTTP API with `curl`

Health check (no token required):

```sh
curl -sS http://127.0.0.1:7117/v1/health
```

```json
{"ok":true,"node":"tower","version":"0.1.0"}
```

List allowed tools for the token's principal:

```sh
curl -sS -H "Authorization: Bearer $TOKEN" http://127.0.0.1:7117/v1/tools
```

Call a tool (`POST /v1/call`):

```sh
curl -sS \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"tool":"meta.whoami","args":{}}' \
  http://127.0.0.1:7117/v1/call
```

```json
{"principal":"phone","role":"observer","door":"http","node":"tower","delegate":false,"paths":["~/projects"],"tools":["..."]}
```

Pass `"on": "pi"` or `"on": "all"` in the JSON body to target remote machines.

### Event stream (SSE)

Add `Accept: text/event-stream` to `POST /v1/call` to receive `stdout`, `stderr`, `progress`, and
`log` events as `event: event` frames, followed by `event: result` or `event: error`:

```sh
curl -sS \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -H "Accept: text/event-stream" \
  -d '{"tool":"meta.version","args":{}}' \
  http://127.0.0.1:7117/v1/call
```

```text
event: result
data: {"name":"legion","version":"0.1.0","protocol":1,"node":"tower","os":"linux","arch":"x86_64"}
```

### MCP over HTTP

Point streamable-HTTP MCP clients at `http://127.0.0.1:7117/mcp` (or your Tailscale Serve URL) with
`Authorization: Bearer <token>`. Add `?compact=1` (or header `Legion-Compact: 1`) for the three
compact meta-tools. `POST` sends JSON-RPC messages and `DELETE` ends a session (`Mcp-Session-Id`).

## Extension tools (`tools.d/`)

Each `.toml` file in `<config_dir>/tools.d/` defines a tool that appears alongside built-in tools
in the CLI, MCP, HTTP, and RPC doors, obeys policy by its declared `effect`, and is audited on
every call.

Define one with `ext.define` (or drop a `.toml` file into `tools.d/` — Legion reloads it live):

```sh
legion ext.define \
  name=ops.greet \
  description="Greet a caller by name." \
  effect=read \
  cmd='printf "{\"hello\":\"%s\"}\n" "$LEGION_ARG_WHO"' \
  params='{"who":{"type":"string","required":true,"description":"Name to greet."}}'
```

```text
name     ops.greet
file     /home/me/.config/legion/tools.d/ops.greet.toml
created  true
```

Call it like any built-in tool:

```sh
legion ops.greet who=world
```

```text
hello  world
```

How arguments and output work:

- Caller arguments are validated against `[params.<name>]` (`type`: `string`, `integer`, `number`,
  or `boolean`; optional `required`, `default`, `enum` for strings, and `description`) before the
  command starts.
- Arguments are never interpolated into a command string. They arrive as `LEGION_ARG_<NAME>`
  (upper-cased) environment variables and as one JSON object on stdin, alongside `LEGION_TOOL`,
  `LEGION_PRINCIPAL`, and `LEGION_NODE`. Any inherited `LEGION_*` environment variables are
  stripped first.
- Standard output is returned directly when it parses as a JSON object, wrapped as
  `{"value": ...}` for other JSON values, or returned as `{"output": "...", "truncated": false}`
  for plain text.
- Use `run = ["prog", "arg1"]` (no shell) whenever possible instead of `cmd`.
- Always declare `effect` honestly (`read`, `write`, `exec`, `destroy`, `power`, `admin`): a role
  permitted that effect can invoke the tool. Built-in names and reserved families cannot be
  shadowed.

Inspect or remove extensions with `ext.list`, `ext.show`, and `ext.remove`. See
[`examples/legion/tools.d/`](../../examples/legion/tools.d/) for complete TOML examples.

## Keeping machines in step (`fleet.sync`)

`fleet.sync` compares, pushes, or pulls shared configuration across machines in `fleet.toml` (all
requiring `effect:admin`, built on `config.list`, `config.read`, and `config.write`):

- **Shared documents**: `fleet.toml`, `policy.toml`, `projects.toml`, and `tools.d/*.toml`.
- **Machine-local (never synced)**: `node.toml` and `tokens.toml`.

Preview or push with `direction=status`, `direction=push`, or `direction=pull from=<machine>`:

```sh
legion fleet.sync direction=push machines='["pi"]' documents='["projects.toml"]' dry_run=true
```

```text
node       tower
direction  push
dry_run    true
results
  pi
    projects.toml
      status       same
      local_hash   blake3:dc454d6962af7a1e26fd258012250ca7d4e3be94c4f1600cf848fa9924308bb5
      remote_hash  blake3:dc454d6962af7a1e26fd258012250ca7d4e3be94c4f1600cf848fa9924308bb5
```

### Conflicts and backups

Each node stores a per-peer BLAKE3 sidecar under `state/sync/sidecars/`. If both nodes edited the
same document since their last sync, `fleet.sync` refuses to overwrite and reports `conflict` with
all three hashes:

```text
node       tower
direction  push
dry_run    false
results
  pi
    projects.toml
      status            conflict
      local_hash        blake3:260bd0f8419834ffe58fac26636ade6980a44682aa98160edfcbdd689e33a2ea
      remote_hash       blake3:e6a6073a032a585b2e9c19c9d2b773d350841fe814d0fddf101cb0155183e298
      last_synced_hash  blake3:dc454d6962af7a1e26fd258012250ca7d4e3be94c4f1600cf848fa9924308bb5
      detail            Remote document changed since the last sync with this machine; pull first or pass force=true to overwrite.
```

Before replacing any file, the receiver validates the incoming TOML with its own strict parser,
saves the previous version under `state/sync/backups/`, writes atomically, and records before/after
hashes in `audit.jsonl`.

## Updating (`meta.update` and `fleet.install`)

*(Live downloads not run here; documented from code.)*

- **Update this machine**:

  ```sh
  legion meta.update dry_run=true
  legion meta.update
  ```

  `meta.update` resolves the newest published `legion-v*` release (or a specific `version=0.2.0`),
  downloads the binary and `SHA256SUMS` using the system `curl` (or PowerShell on Windows),
  verifies SHA-256, caches the verified binary under `state/dist/<version>/<target>/`, and
  atomically replaces the running executable. On Windows, the running binary is renamed aside to
  `.old.*` and cleaned up on the next startup.

- **Install or upgrade another SSH machine**:

  ```sh
  legion fleet.install machine=pi dry_run=true
  legion fleet.install machine=pi
  legion fleet.install machine=pi from=target/release/legion
  ```

  `fleet.install` probes the remote OS and CPU architecture over `ssh`, downloads and verifies the
  matching release (reusing `state/dist/` when already cached) or uses the local file given in
  `from`, copies it over `scp` to a random temporary name beside `~/.local/bin/legion` (or
  `%LOCALAPPDATA%\legion\legion.exe` on Windows), atomically renames it into place, and opens a
  normal `legion rpc` session to verify the new version.
