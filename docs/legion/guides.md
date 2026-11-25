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
  Extra files are deleted only **after** the new content has arrived (except entries that must be
  removed up front to make room for a kind change, such as replacing a file with a directory).
  Anything matched by `--exclude` is ignored at both ends and never deleted.
- `--checksum` compares files by BLAKE3 hash instead of size and modification time (reads every
  file at both ends) and verifies each unpacked file against its planned hash before replacing the
  destination.
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

Progress is rendered on one line of `stderr` when `stderr` is a terminal. Bytes move over a
dedicated `legion stream` pipe (never inside JSON), relayed through the calling node. Both ends
exchange a `{"protocol": 1}` line first (60 s handshake/first-reply timeout, 120 s idle timeout,
with `{"busy": true}` heartbeats every 10 s during long work); directory `unpack` receives the
planned `entries` (`path`, `kind`, `size`, `hash`, `target`) and refuses anything else in the tar
stream. Every operation is checked against the target machine's `policy.toml` (`fs.read`,
`fs.write`, or `fs.remove`) and written to its audit log with door `stream`.

### Transfers involving a `via` relay

Because `legion stream` opens a direct pipe to each end, a machine configured with `via = "pi"`
cannot be an endpoint of a transfer started on `tower`:

```sh
legion fs.transfer from=~/projects/site/README.md to=shed:~/README.md dry_run=true
```

```text
legion: unsupported: Machine shed is reached via relay pi, so a direct pipe cannot be opened to it.
  Run transfers involving shed on pi instead (for example `legion fs.transfer --on pi from=... to=...`), or configure a direct ssh or command transport for shed in fleet.toml.
```

Run the transfer on the relay node (`legion fs.transfer --on pi ...`), or give `shed` a direct
`ssh` or `command` entry.

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

### Jobs on Windows

To survive after an SSH session disconnects, a background job has to run outside the Windows Job
Object that OpenSSH attaches to every incoming session. When `job.start` runs locally and Windows
allows job breakaway, Legion starts the job supervisor directly in its own named Job Object. When
breakaway is refused (the normal case inside an OpenSSH session), Legion asks the Windows
Management Instrumentation service (`Winmgmt`) to launch the supervisor via `Win32_Process.Create`
with breakaway enabled so it runs outside both the SSH session's job and the WMI provider host's
job.

If `job.start` fails saying Windows did not answer or that `Winmgmt` is unavailable, the WMI
service is stopped, stalled, or the machine is heavily loaded. Legion refuses to fall back inside
the SSH session's job by default because the job would be killed as soon as SSH disconnects. To
recover:

1. Check `job.status` (or `job.list`) first: if the launch timed out after Windows already started
   the request, confirm whether the job is running or marked failed before retrying.
2. On the Windows machine, check the WMI service from an elevated PowerShell prompt and start it if
   stopped:

   ```powershell
   Get-Service Winmgmt
   Start-Service Winmgmt
   ```

3. Retry `job.start` with the same `key` once `Winmgmt` is running and system load has settled.
   Passing `key` ensures a retry never starts a second copy while an earlier launch is still in
   flight.

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
so the reply leaves first (`power.restart` and `power.shutdown` take `delay_s` up to 600 seconds),
every disruptive action passes through the busy gate, and until it has happened `power.cancel`
calls it off.

### Busy, inhibitors, and one action at a time

A node reports one of three busy states in `power.status`:

- `unmonitored` — `node.toml` configures no `busy` setting. Unattended power calls fail with `busy`
  because not knowing is not the same as idle.
- `busy` — at least one `[[busy.probes]]` exited `0` (or failed/timed out — fail closed) or an
  `exclusive` job is running.
- `idle` — `busy = "none"` is declared explicitly, or every probe exited `1` and no exclusive job
  is running.

Unless the call says `force=true`, the busy state is checked when the call is made and again just
before the action. On Linux and macOS the delayed runner asks the probes and exclusive jobs once
more when the delay has passed, and does nothing if the machine has become busy meanwhile (`last`
then shows `fate: busy` with what kept it busy). On Windows, whose own timer carries out a restart or
shutdown, the state is checked only when it is handed over, and the reply's `warnings` say so: with
a long `delay_s` there, use `power.cancel` if work starts before it is due.

Three additional guards protect the machine:

- **One power action at a time**: a node holds two kernel locks under `state/power/` (`call.lock`
  during the call and `runner.lock` in the delayed runner until the action has run, plus a settle
  window after sleep). A second power call while one is pending fails with `conflict` instead of
  queuing a second sleep or re-arming firmware underneath the first.
- **Linux logind inhibitors**: if a Linux application holds a logind sleep or shutdown inhibitor,
  the call fails with `busy` and names who holds it. `force=true` overrides busy probes and
  `unmonitored`, **not** logind inhibitors; to override inhibitors, pass `ignore_inhibitors=true`
  (which passes `-i` to `systemctl` and requires `[power] sudo = true`).
- **`power.status` `pending` and `last`**: while an action is waiting to run, `power.status`
  includes `pending`, and `meta.doctor` adds a warning naming it and `power.cancel`. After it runs (or after a reboot), `last` reports how the previous handed-over
  action ended — either the runner's own record (`ran_at`, `exit_code`, `said`, and `disarmed` if a
  failed `into` restart cleared the firmware's next boot) or a derived `fate` (`pending`,
  `happened`, `did_not_happen`, `runner_gone`, `given_up`, `busy` when the machine was busy when it
  was due, or `cancelled`, which also names `cancelled_by` and `cancelled_at`).

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

`dry_run=true` always returns the exact `commands` that would run and includes a `would_refuse`
object (`code`, `message`, `hint`) whenever a check — busy gate, unmonitored node, pending action,
privilege check, or Linux logind inhibitor — would refuse the real call:

```sh
legion power.sleep force=true dry_run=true
```

```text
action     sleep
scheduled  false
in_s       3
boot_id    00000000-0000-4000-8000-000000000001
commands
  - systemctl, suspend
dry_run    true
would_refuse
  code     busy
  message  Refused to put this machine to sleep: logind is holding it back for Media Player (Playback in progress, pid 1234).
  hint     Wait until they let go, or pass ignore_inhibitors: true to override them (with [power] sudo = true). force does not: an inhibitor is something on the machine asking not to be interrupted.
```

When all checks pass, `would_refuse` is omitted:

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

### Calling a pending action off (`power.cancel`)

A mistaken `power.restart` or `power.shutdown` can be called off until it happens, from the machine
itself or through another with `--on`. Give a longer `delay_s` when you want time to change your
mind:

```sh
legion power.restart delay_s=600 --on tower
legion power.cancel --on tower
```

```text
cancelled  true
action     restart
id         187a9f3c2b1d4e00-10e1
was_due    2026-11-24T10:10:00Z
detail     Called off the restart handed over at 2026-11-24T10:00:00Z, which was due at 2026-11-24T10:10:00Z: it will not run. Its runner (process 4321) was stopped.
```

On Linux and macOS the delayed runner that would have run the command is stopped, by the identity
recorded when it started; on Windows, which times a restart or shutdown itself, Windows calls it
off with `shutdown /a`, and its own answer decides what `power.cancel` reports. For a restart into
another system, the one-time boot entry it armed is cleared again and read back: for an `efi_label`
target with `efibootmgr --delete-bootnext` or `bcdedit /deletevalue`, for a target with `arm` and
`verify` commands only when it also gives a `disarm` command (whose clearing `verify` must then stop
confirming). Without one, the reply says the firmware stays set to start that system on the next
boot, whatever causes it; the same happens when a restart fails after arming. Afterwards `power.status`
shows no `pending` action and `last` with `fate: cancelled` and who cancelled it, and the next power
call can go ahead.

`power.cancel` reports `cancelled: false` with a `detail` instead of failing when there is nothing to
call off: nothing is pending (the detail says what became of the last action), Windows says no
shutdown is in progress because someone called it off on the machine, or the runner has already
started the command, which can no longer be stopped from here. A sleep is a few seconds out, so in
practice it is too late to call off. `power.cancel` needs the `power` effect, like the actions it
calls off.

### Restarting into another system (`power.restart` and `power.switch`)

`power.restart into=windows` resolves the firmware entry by label on every call (never cached),
arms one-time boot (`efibootmgr --bootnext` on Linux, `bcdedit` on Windows, or your custom `arm`
and `verify` commands, with an optional `disarm` that clears it again), reads the setting back, and
schedules the reboot only if the readback matches:

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

Before relying on unattended power or dual-boot switching, test each machine kind once while you can
walk over to it. Always start with `legion power.status` and `dry_run=true` on every call and check
that `would_refuse` is absent:

- **Linux desktop or workstation**:
  1. Configure `[[busy.probes]]` (or `busy = "none"`) and, if suspending/rebooting over headless
     SSH without an active polkit session, set `[power] sudo = true` with narrow `NOPASSWD` sudoers
     rules for `systemctl suspend`, `systemctl reboot`, `systemctl poweroff`, and (if overriding
     inhibitors) `systemctl -i ...`.
  2. Check `legion power.sleep dry_run=true` both when idle and while a desktop app holds a logind
     inhibitor so you see how `would_refuse` reports it.
  3. Run one supervised `power.sleep` and confirm it suspends, runs `[power] before_sleep`, resumes,
     and records `last` in `power.status`.
- **Linux headless server or Raspberry Pi**:
  1. Confirm `sudo -n -l -l /usr/bin/systemctl reboot` succeeds without a password if `[power]
     sudo = true`.
  2. Run one supervised `power.restart` and confirm `boot_id` changes in `power.status` when it
     returns.
- **macOS** *(a restart and a shutdown are scheduled and called off in CI; none has been carried out)*:
  1. `power.sleep` uses `pmset sleepnow` (no `sudo` required); `power.restart` and `power.shutdown`
     use `shutdown -r now` / `shutdown -h now` and need `[power] sudo = true` with `NOPASSWD`
     sudoers rules (or root).
  2. Verify `power.sleep dry_run=true` and `power.restart dry_run=true` have no `would_refuse`,
     then run one supervised sleep and one supervised restart.
- **Windows** *(a restart and a shutdown are scheduled and called off in CI; none has been carried out)*:
  1. `power.restart` and `power.shutdown` use `shutdown.exe /r /t <delay>` and `/s /t <delay>`, so
     Windows times the delay itself and `power.status` tracks `last.fate` across reboots.
  2. For `power.sleep`, place Sysinternals `psshutdown` on `PATH` (or set `[power] sleep` in
     `node.toml`), verify `power.sleep dry_run=true`, and run one supervised sleep to confirm the
     machine actually enters sleep and wakes cleanly.
- **Dual-boot machines**:
  1. Set `system` in each OS's `node.toml` and matching `[[machines.<name>.systems]]` in
     `fleet.toml`, plus `[boot.<other>]` (`efi_label` or custom `arm`/`verify`).
  2. Verify `efibootmgr` (Linux, with `sudo -n`) or `bcdedit` (Windows elevated SSH) via
     `power.restart into=<other> dry_run=true`.
  3. Run one supervised `power.switch machine=<name> into=<other>` from another machine, confirm it
     lands in `<other>` with a new `boot_id`, then reboot plainly and confirm the firmware returns
     to the default OS.
- **Wake helpers (`wake_via`)**:
  1. Set `mac` on the helper's own `fleet.toml` and `wake_via` on the caller's `fleet.toml`.
  2. Run `power.wake machine=<name> dry_run=true` to check the broadcast interface and address,
     then run one supervised `power.wake machine=<name> wait_s=90` while the target is asleep to
     confirm BIOS/NIC Wake-on-LAN and LAN broadcast delivery.

## Pair a phone or tablet (`legion pair`)

A phone, tablet, or other controller pairs with a node once and belongs to the whole fleet: once
paired, it connects over SSH to any synced machine in `fleet.toml`, confined to its principal by
`legion gate --as <principal>`.

Run `legion pair` in your terminal:

```sh
legion pair
```

When run interactively without flags, `legion pair` asks for a device name and lists the roles
defined in `policy.toml` (plus the built-in `owner` role, with explicit confirmation) with a
plain one-line description of what each role allows. No role or device name is built in. You can
also pass flags directly, or pair under an existing principal:

```sh
legion pair --name pixel --role observer
legion pair --name tablet --role owner --allow-owner
legion pair --as phone
```

When pairing with `--name` and `--role` (or interactively), Legion reserves the device name in the
pending invite and creates its dedicated `[principals."device:<name>"]` entry (marked with
`device = "<pairing id>"`) atomically in `policy.toml` when the device enrols, so two devices never
share an identity and an unused invite leaves `policy.toml` untouched.
`legion pair` calls `device.invite`, prints who the device will act as and when the invite expires,
draws the invite as a terminal QR code, prints the invite text on one line (for copying and pasting
when a camera is inconvenient), and prints the commands to list and remove devices. Pass `--invert`
if your light-background terminal needs inverted QR polarity, `--valid 1h` (up to `24h`) to change
the 15-minute default, `--host`, `--port`, or `--user` to override detected connection details, or
`--json` for the raw `device.invite` output.

What happens during pairing:

1. `device.invite` generates a fresh single-use Ed25519 key pair and appends one line to
   `authorized_keys` carrying `restrict`, a portable 12-digit `expiry-time` upper bound (compatible
   with OpenSSH 8.9+), and `command="<legion> gate --enrol <id> ..."` (always pinning the node's
   resolved `--home` or `--config-dir`/`--state-dir` so SSH environment variables cannot redirect
   the gate, plus `--account` on Windows's shared `administrators_authorized_keys`). The invite
   carries the node's detected Tailscale MagicDNS name and Tailscale addresses (or the system
   hostname when Tailscale is absent), the account and port, the node's sshd public host keys
   (pinned — when `[devices] host_keys` is unset, `ssh-keyscan` against local `sshd` is intersected
   with readable default host `.pub` files as `ssh-keyscan+files`, refusing disjoint results with a
   `[devices] host_keys` hint, and falling back to scan alone — labelled `ssh-keyscan only`, with a
   warning in `legion pair` — or files alone only when the other is unavailable; invites without a
   usable host key are refused), and the temporary key. Neither the invite text nor the temporary
   key is logged or audited.
   **Keep the invite secret until used:** `device.invite` returns the invite text (which carries
   the temporary private key seed) in its tool result, so invoking `device.invite` through an AI
   agent puts the live invite into that agent's conversation transcript. Run `legion pair` directly
   in your own terminal whenever you do not want an agent transcript to see the invite.
2. The device scans the QR code or pastes the invite, connects over SSH verifying the pinned host
   key, and sends its own newly generated Ed25519 public key and name.
3. `legion gate --enrol <id>` validates the invite and public key under locks on shared config and
   `authorized_keys`, enforces exact UTC expiry from `state/devices.json` (an expired temporary
   key stops working at its exact expiry because `legion gate --enrol` refuses it and the expired
   line is swept on the next `device.*` command), refuses enrolling the invite's own temporary key
   or any key already present in `authorized_keys` or `devices.toml`, writes the device to
   `devices.toml` beside `policy.toml`, reconciles `authorized_keys` so the temporary line is
   replaced by a permanent
   `restrict,command="<legion> gate --as <principal> --device <name> --pairing <id> ..."` line
   pinning a random per-pairing id and the node's config and state directories, records non-secret
   metadata in `state/devices.json`, pushes `devices.toml` in parallel to reachable machines in
   `fleet.toml` (reporting per-machine outcomes under `fleet`), and writes an audit record
   (`door: "ssh"`, `tool: "device.enrol"`, `device: "<name>"`). The temporary key immediately stops
   working. Every subsequent call served through `legion gate` is reported and audited with
   `door: "ssh"` (and `device: "<name>"` when `--device` is set).

### Managing devices across the fleet (`device.list`, `device.set`, `device.endpoint`, `device.remove`)

Paired devices are stored in `devices.toml` beside `policy.toml`, and each node derives its own
`authorized_keys` device lines from `devices.toml`. On first run, Legion adopts only pre-existing
`--pairing` lines verified against this node's own `state/devices.json` (equal principal, key
fingerprint, and pairing id); any other Legion-shaped line stays local-only (kept working locally,
never synced to peers, flagged as `local_only: true` in `device.list`, refused by `device.set`,
and removable via `device.remove`). Foreign `authorized_keys` lines, pending invites, and entries
belonging to other Legion homes or OS accounts are preserved byte for byte.

**Two-writers and sync trust rules:** in steady state, only two things ever write `devices.toml` —
the node's own device tools (`device.invite` + enrolment, `device.set`, `device.remove`), or an
owner-authorised write (`config.write devices.toml` and the `devices.toml` part of `fleet.sync`
`push`/`pull` require the `owner` role on the node doing the writing, locally and on the receiving
peer; for non-owner callers `config.write devices.toml` is refused and `fleet.sync` skips
`devices.toml` with the reason while syncing the other documents). Adoption of older single-machine
pairings is a third, bootstrap-only writer that runs only when `devices.toml` does not exist yet
and builds entries strictly from this node's own `state/devices.json` records matching
`authorized_keys`. After a local device tool succeeds, the node pushes `devices.toml` to reachable
peers with its own `owner` authority and records `device_registry_push` audit metadata on both
nodes naming who triggered it (leaving `device` and `pairing_id` empty for CLI-created invites).
Pushing (`fleet.sync direction=push`, and automatic pushes after enrolment, `device.set`, and
`device.remove`) sends local entries to peers and imports **only tombstones** back — never live
device entries from a peer. To import live devices paired on another machine, run `legion
fleet.sync direction=pull from=<machine> documents='["devices.toml"]'` explicitly as owner
(`fleet.sync direction=status` compares read-only).

- **List devices and invites**: `legion device.list` reconciles `authorized_keys` with
  `devices.toml`, sweeps expired invites recorded by this home, and shows each device's `id`,
  `name`, `principal`, effective `role`, `fingerprint`, `added`, `paired_on`, and any collision,
  immutable-field conflict, role-tie conflict, unmarked-principal, or quarantine `status`. For owner callers, it also
  shows Legion lines pointing at another or deleted Legion home (`foreign: true`,
  `missing_home: true`, `home`). `meta.doctor` warns about orphaned keys, unmarked `device:*`
  principals, key collisions, conflicting or role-conflicting device entries, quarantined devices, and drifted fleet
  peers (and fails loudly if a previous `policy.toml`/`devices.toml` rollback failed; after
  verifying the files, an owner clears that record with `legion meta.doctor
  acknowledge=devices-rollback`). Tombstones in `devices.toml` are permanent so removed devices
  cannot return. Pure tombstone saves and merges are never refused for size; any change adding or
  altering a live entry must fit under 1 MiB with every live entry counted at its worst-case
  tombstone size (1,049 bytes, guaranteeing at least 999 lifetime fleet devices even at maximum
  field lengths so removing every live device always fits), non-owner callers are capped at 32
  pending invites per principal and at 512 KiB (with the same reserve), and owner
  `legion device.remove` is never blocked by size.
- **Change role, rename, or confirm**: `legion device.set` changes a paired device's role (owner
  role only, only for a registry-owned `device:*` principal marked with `device = "<pairing id>"`,
  requiring `allow_owner=true` to raise to `owner`, rolling back `policy.toml` if saving
  `devices.toml` fails, and refusing expiring credentials), renames it, or confirms a quarantined
  descendant (`confirm=true`, owner role only), updating `policy.toml`, `devices.toml`, and
  `authorized_keys`, then pushing `devices.toml` to reachable fleet peers. `name` and `role` carry
  independent update stamps so a stale rename never raises a role lowered on a peer, and a stale
  role change never undoes a newer rename; an equal-stamp, equal-origin role tie marks
  `role_conflict = true` (inactive until an owner resolves it with `device.set id=<id> role=<role>`),
  while `device.set` refuses entries flagged with an immutable conflict until they are removed.
- **Discover endpoints**: `legion device.endpoint` (`effect: read`) returns this node's SSH
  connection details (`node`, `hosts`, `port`, `user`, `host_keys`) using the same detection as
  `device.invite` (caching auto-discovered host-key scans briefly per node). Controllers call
  `device.endpoint` with `on: "all"` through any reachable node to learn every machine's direct SSH
  address and pinned host keys.
- **Remove a device**: `legion device.remove` accepts `name=<device>` or `id=<pairing-or-invite-id>`,
  writes a tombstone carrying the removed device's provenance to `devices.toml` first before
  cleaning up its unused registry-owned
  `device:*` principal and `authorized_keys` line, immediately terminates any active sessions and
  verified same-account parent `sshd` connections (disambiguated by pairing id, including
  multiplexed `ControlMaster` connections, reporting `sessions_ended`, even if a later step fails
  after saving `devices.toml`), reports any still-running background jobs started by that device in
  `jobs_left_running` without killing them, and pushes the tombstone to reachable peers in
  `fleet.toml` with the node's own authority and both-sides audit. Any live descendant created by a
  removed device at or after `removed_at - 10 minutes` (and transitively its unconfirmed
  descendants) is automatically held back (quarantined) until an owner runs `device.set id=<id>
  confirm=true`; this hold-back assumes the machines' clocks agree within ten minutes.
  **Honest limit without a central hub:** a removed device that held an `owner` or `exec` role can,
  on any machine that has not yet heard about the removal, still act with that machine's own
  authority until the tombstone arrives; removal is complete only once every machine has received
  the tombstone, and `device.remove` names every machine that was not reached in `still_trusted_by`
  and prints the `fleet.sync` command to finish revoking once they return. On refusal, `legion gate`
  verifies the on-disk key and `devices.toml` for its exact `--device`, `--pairing` id, and
  principal after registering and tears down its verified same-account parent `sshd` only on
  `Denied`, so neither a newly opened channel on an already-open connection, a stale key on disk,
  nor a later re-pair under the same name can revive a removed pairing, while transient local config
  errors do not kill an unrelated healthy connection. Owner callers can clean up foreign/orphaned
  Legion lines with `device.remove ... foreign=true` (reporting `sessions_not_ended: true`).

```sh
legion device.list
legion device.set name=pixel role=editor
legion device.set name=pixel rename=pixel-10
legion device.set id=<pairing-id> confirm=true
legion device.endpoint --on all
legion device.remove name=pixel-10
legion device.remove id=<pairing-or-invite-id>
legion device.remove name=old-phone foreign=true
```

### Overriding `authorized_keys`, host keys, and endpoint details (`[devices]`)

By default Legion edits `~/.ssh/authorized_keys` (on Windows, when the account is in the
`Administrators` group and `sshd_config` uses `administrators_authorized_keys`, it edits
`%ProgramData%\ssh\administrators_authorized_keys`, sets its owner to `BUILTIN\Administrators`
alongside the `Administrators` and `SYSTEM` ACL, pins `--account` on forced commands so each
administrator sees and manages only their own entries, and quotes forced commands for the shell
configured in `HKLM\SOFTWARE\OpenSSH\DefaultShell` — `cmd.exe` by default, PowerShell (refusing
Unicode curly single-quote delimiters `U+2018`–`U+201B`), or a POSIX shell such as Git Bash,
refusing unrecognised shells clearly). Host keys intersect `ssh-keyscan`
against local `sshd` on the invite's port with `/etc/ssh/ssh_host_*_key.pub` (or
`%ProgramData%\ssh\ssh_host_*_key.pub` on Windows), refusing disjoint keys unless configured
explicitly. To override paths or advertised endpoint details (`hosts`, `port`, `user`) for a custom
sshd setup, set `[devices]` in `node.toml`:

```toml
[devices]
authorized_keys = "/home/me/.ssh/authorized_keys"
host_keys = ["/etc/ssh/ssh_host_ed25519_key.pub"]
hosts = ["tower.example.ts.net"]
port = 22
user = "me"
```

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
inspect tokens, and `legion token remove phone` to revoke one (revoking also closes any active MCP
sessions opened with that token).

Token management enforces three guard rails:

- A caller may create, list, or remove tokens only for a principal it could itself act as (its own,
  or any principal when the caller has the `owner` role).
- Creating a token for any principal whose role is `owner` requires a caller with the `owner` role
  and `allow_owner=true`.
- A token created through an expiring HTTP token cannot outlive the caller's token.

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
ui = true
```

`legion serve` serves the built-in web interface at `/` (no token is needed to load the static page
itself; every `/v1/*` and `/mcp` request still requires a bearer token). Set `ui = false` under
`[serve]` to serve only the HTTP API and MCP endpoints.

Requests with any unlisted `Host` are refused (stopping DNS rebinding), and requests with an
unlisted cross-origin `Origin` are refused with `403` (stopping random browser tabs from calling the
loopback port).

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

### File content (`POST /v1/content`)

To feed a file to an `<img>`, `<video>`, `<audio>`, or resumable download with byte-range access,
mint a short-lived content address with `POST /v1/content`:

```sh
curl -sS \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"path":"~/projects/app/clip.mp4","on":"pi"}' \
  http://127.0.0.1:7117/v1/content
```

```json
{"url":"/v1/content/<ticket>/clip.mp4","expires_in":900}
```

Omit `"on"` to read a file on the node serving HTTP itself; when `"on"` names another machine in
`fleet.toml`, the node reads the requested byte range through its own `legion stream` session to
that machine. `"on"` must name a single machine (never `"all"` or a list).

Fetch the returned same-origin address with `GET` or `HEAD`, with optional single-range `Range`,
`If-Range` (matched against the strong `ETag` built from size and modification time), and
`?download=1` to force `Content-Disposition: attachment`:

```sh
curl -sS -r 0-65535 http://127.0.0.1:7117/v1/content/<ticket>/clip.mp4 -o chunk.bin
curl -sSI 'http://127.0.0.1:7117/v1/content/<ticket>/clip.mp4?download=1'
```

Security and lifecycle guarantees:

- **Tickets and revocation**: 256 random bits bound to one bearer token, one principal, one
  machine, and one bounded file path (at most 4 KiB; `POST /v1/content` bodies are capped at 16
  KiB). Permissions are checked as `fs.read` when the ticket is minted, on every `GET` or `HEAD`,
  and periodically while streaming (every 2 MiB or 2 seconds), so revoking the token, tightening
  `policy.toml`, or crossing the 6-hour hard limit cuts off active downloads. Tickets expire using
  a suspend-safe clock (15 minutes after last use, 6 hours absolute maximum), are capped at 128 per
  token (evicting the oldest), and are never logged; minting writes one `content.ticket` audit
  entry and successful reads coalesce to at most one `content.read` (and `stream.get` on remote
  nodes) per ticket per minute with request count and bytes served.
- **Headers and isolation**: every reply carries `X-Content-Type-Options: nosniff`,
  `Cross-Origin-Resource-Policy: same-origin`, `Referrer-Policy: no-referrer`, `Cache-Control:
  private, no-store`, and unconditionally
  `Content-Security-Policy: sandbox; default-src 'none'; frame-ancestors 'none'`. `Content-Type`
  comes from a fixed allow-list (raster images, SVG, video, audio, `text/vtt; charset=utf-8` for
  `.vtt`, and `text/plain; charset=utf-8` for all other text files including HTML); unlisted types
  — including PDF — are served as `application/octet-stream` with `Content-Disposition:
  attachment`. Opened HTML, SVG, or PDF files can never run script or be framed inside the
  interface's origin.
- **Host, `Origin`, and third-party pages**: `/v1/content/<ticket>/<name>` enforces the same `Host`
  and `Origin` checks as every other route. Because `<img>`, `<video>`, `<audio>`, and direct
  downloads send no `Origin` header, an absent `Origin` is accepted on `GET` and `HEAD`; if a
  browser sends an `Origin`, it must match the same origin or `[serve] origins` in `node.toml`,
  otherwise the request is refused with `403`. Because `Cross-Origin-Resource-Policy` is always
  `same-origin` on the HTTP door, listed cross-origin pages must fetch content using CORS
  (`fetch()` or `crossorigin`) rather than no-cors embedding. Against a third-party page that
  somehow learns a ticket, secrecy of the 256-bit ticket is the first barrier, while `Origin`
  refusal, `Cross-Origin-Resource-Policy: same-origin`, `frame-ancestors 'none'`, and `sandbox;
  default-src 'none'` block cross-origin reads, no-cors embeds, framing, and script execution.
- **History and exposure**: a content address remains usable for up to 6 hours while in active use
  and appears in browser history if opened directly in a tab; damage is limited because it grants
  read-only access to one file, expires after 15 minutes idle or 6 hours total, stops immediately
  when the token or `fs.read` policy is revoked, and never caches or executes in the interface's
  origin.
- **Concurrency and slow-reader protection**: active content streams are capped per ticket (8), per
  holder (16), and globally (64), answering `429 Too Many Requests` with `Retry-After` and
  `Connection: close`, and stalled or overly slow readers are disconnected after a write-idle
  timeout (15 s) or minimum transfer rate window (40 KiB / 10 s) so API and UI routes stay
  responsive. Remote reads reuse pooled `keep_open` `legion stream` pipes and multiplexed SSH
  connections (`ControlMaster` on Unix), capped at 8 concurrent streams per target machine.

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

When built from git (or with `LEGION_BUILD_REV`), `meta.version` and `hello` also include an
optional `build` field (`"<12-character commit>"` or `"<12-character commit>, modified"`), matching
the revision printed in parentheses by `legion --version`.

### MCP over HTTP

Point streamable-HTTP MCP clients at `http://127.0.0.1:7117/mcp` (or your Tailscale Serve URL) with
`Authorization: Bearer <token>`. Add `?compact=1` (or header `Legion-Compact: 1`) for the three
compact meta-tools. `POST` sends JSON-RPC messages and `DELETE` ends a session (`Mcp-Session-Id`).

Each session belongs to the token that opened it (other tokens cannot use its `Mcp-Session-Id`). A
token may hold up to 16 live sessions at once (opening a 17th closes its least recently used
session), the server caps total live sessions at 256, idle sessions expire after 5 minutes, and
removing, replacing, or expiring a token closes all of its sessions immediately.

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
- **Large string arguments**: string arguments larger than 32 KiB per variable or 96 KiB total are
  refused by default (`large = "refuse"`). Set `large = "stdin"` on tools that read the full value
  from stdin JSON: Legion omits the oversized `LEGION_ARG_<NAME>` environment variable, sets
  `LEGION_ARG_<NAME>_OMITTED=1`, and passes the full value on stdin.
- Standard output is returned directly when it parses as a JSON object, wrapped as
  `{"value": ...}` for other JSON values, or returned as `{"output": "...", "truncated": false}`
  for plain text.
- Use `run = ["prog", "arg1"]` (no shell) whenever possible instead of `cmd`.
- Always declare `effect` honestly (`read`, `write`, `exec`, `destroy`, `power`, `admin`): a role
  permitted that effect can invoke the tool. Built-in names and every family registered in the
  built-in registry (plus reserved families `agent`, `audit`, `config`, `device`, `exec`, `ext`,
  `fleet`, `fs`, `job`, `legion`, `meta`, `power`, `proj`, `svc`, `sys`, `token`) cannot be
  shadowed.
- Full-mode MCP sessions receive `notifications/tools/list_changed` when `tools.d/` changes.

Inspect or remove extensions with `ext.list`, `ext.show`, and `ext.remove`. `ext.list` prints an
effect-honesty `note` and each tool's execution `mode` (`run` or `cmd`):

```sh
legion ext.list
```

```text
note    A tool declared `read` is callable by every role that may read, whatever its command does, so declare each tool's effect honestly.
tools
  - name         ops.greet
    description  Greet a caller by name.
    effect       read
    mode         cmd
    file         /home/me/.config/legion/tools.d/ops.greet.toml
    params       who
broken  -
```

See [`examples/legion/tools.d/`](../../examples/legion/tools.d/) for complete TOML examples.

## Keeping machines in step (`fleet.sync`)

`fleet.sync` compares, pushes, or pulls shared configuration across machines in `fleet.toml` (all
requiring `effect:admin`, built on `config.list`, `config.read`, and `config.write`):

- **Shared documents**: `devices.toml`, `fleet.toml`, `policy.toml`, `projects.toml`, and
  `tools.d/<name>.toml`. Extension files may contain internal dots (matching `ext.define` files
  such as `tools.d/ops.greet.toml`); on write, the receiver enforces the same rules as `ext.define`
  — the filename stem must match the `name` field inside the file, duplicate tool names across
  files are refused, and reserved families are rejected. `devices.toml` obeys the two-writers rule
  (push and pull require the `owner` role on both nodes; non-owner `fleet.sync` skips `devices.toml`
  with the reason while syncing the other documents) and merges per field (`name` and `role` carry
  independent stamps, disagreements on immutable identity or provenance fields are flagged as
  conflicts with no `authorized_keys` line, tombstones always win, take identity fields from local
  entries on merge, and are never refused for size, local confirmations are kept when identity and
  pairing provenance match or cleared with a warning otherwise, registry-owned `device:*`
  principals and `authorized_keys` are reconciled, and active sessions for newly removed,
  conflicted, or quarantined devices are ended) while obeying the one-way trust rule:
  `direction=push` sends local entries to peers and imports **only tombstones** back (never live
  peer entries), whereas `direction=pull from=<peer>` imports live devices from that named peer
  without writing back. `meta.doctor` warns if any reachable peer has a different `devices.toml`.
- **Machine-local (never synced)**: `node.toml` and `tokens.toml`.

Inspect differences with `direction=status` (reporting per-document status `same`, `only_here`,
`only_there`, `changed_here`, `changed_there`, or `both`):

```sh
legion fleet.sync direction=status machines='["pi"]'
```

```text
node       tower
direction  status
dry_run    false
results
  pi
    fleet.toml
      status      only_here
      local_hash  blake3:91098d9781e8e315bedfc42355c770ee4b9e8ffb73c7a43422fe17aed20dc667
    policy.toml
      status      only_here
      local_hash  blake3:02737aa64498159b5cfe945119648617b86c513c651f3e995727fe08c7b562e1
    projects.toml
      status  same
    tools.d/ops.greet.toml
      status      only_here
      local_hash  blake3:420c9d834cb6b1c84dc3aeb8572afb2a7bdee9957b56a711d32be58413120695
```

Push or pull with `direction=push` or `direction=pull from=<machine>` (reporting `same`, `pushed`,
`pulled`, `conflict`, or `error`):

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

## Updating and releasing (`meta.update` and `fleet.install`)

*(Live downloads not run here; documented from code.)*

Both `meta.update` and `fleet.install` verify every release download (and every cached release under
`state/dist/<version>/<target>/`) against the offline Ed25519 signature `SHA256SUMS.sig` using the
release public key compiled into `legion` from `contract/release-public-key.pem`. There is no
runtime override or skip flag: unsigned or tampered releases are always refused.

Both tools also enforce a persistent SemVer downgrade floor (per node for `meta.update`, per machine
for `fleet.install`) alongside the running version, so a validly signed older release is refused
unless you pass `allow_downgrade=true`. Version checks compare semantic versions rather than build
revisions, so reinstalling the same version from a newer commit succeeds; when the installed binary
reports a `build` revision, both tools include `build` in their result. Before replacing any live
binary, both check the executable format header (ELF/Mach-O/PE and architecture), run `--version` on
the staged file, and save a one-generation `<exe>.prev` backup beside the destination.

- **Update this machine**:

  ```sh
  legion meta.update dry_run=true
  legion meta.update
  legion meta.update from=/path/to/legion
  ```

  `meta.update` resolves the highest published stable `legion-v*` semantic version (or a specific
  `version=0.2.0`), or installs a local binary you vouch for with `from=<local file>` (no signature
  needed; cannot be combined with `version`). For releases, it selects native arm64 on macOS under
  Rosetta 2 and x86-64 under emulation on Windows ARM64 (with a `note`), downloads over HTTPS/TLS
  1.2 via `curl` (or PowerShell on Windows), and verifies `SHA256SUMS.sig` and SHA-256. In both
  modes it checks the binary header, stages and runs `--version` preflight, enforces the downgrade
  floor and `allow_downgrade`, saves `<exe>.prev`, atomically replaces the running binary, and
  verifies the replaced binary runs (restoring `<exe>.prev` automatically if it does not). On
  Windows, the running executable is renamed aside to `.old.*` (restored if the replacement move
  fails, otherwise cleaned up on the next startup).

- **Install or upgrade another SSH machine**:

  ```sh
  legion fleet.install machine=pi dry_run=true
  legion fleet.install machine=pi
  legion fleet.install machine=pi from=/path/to/legion
  ```

  `fleet.install` probes the remote OS, architecture, and destination path over `ssh`, downloads and
  verifies the signed release (reusing `state/dist/` after re-verifying its signature and digest) or
  uses an unsigned self-built binary you vouch for with `from=<local file>`, checks the downgrade
  floor and binary header locally, copies to a random temporary file over `scp`, runs `--version`
  preflight on the remote host before saving `<dest>.prev` and moving it into place, and opens a
  fresh `legion rpc` session to confirm the installed version (reporting its `build` when present).
  If the new session fails or reports a different version, `fleet.install` restores `<dest>.prev`
  automatically.

### Publishing a release (owner procedure)

The GitHub Actions workflow `.github/workflows/legion-release.yml` never holds the signing key and
never signs releases; the offline Ed25519 signature is created locally before publishing:

1. Bump `[workspace.package].version` in `Cargo.toml` on `main` and wait for CI to pass.
2. Tag that commit on `main` and push the tag:

   ```sh
   git tag legion-vX.Y.Z && git push origin legion-vX.Y.Z
   ```

3. Wait for the `Legion release` workflow to finish building all five targets, attaching GitHub
   build provenance attestations, and creating the draft release (with `--latest=false` so
   `/releases/latest` stays untouched for older apps).
4. On the machine holding `~/.legion-control/release-signing.key`, sign `SHA256SUMS` offline and
   upload `SHA256SUMS.sig` to the draft release:

   ```sh
   node scripts/sign-legion-release.mjs legion-vX.Y.Z
   ```

   The script downloads `SHA256SUMS` and every release asset, re-hashes them locally, checks that
   the signing key matches `contract/release-public-key.pem`, writes and verifies `SHA256SUMS.sig`,
   uploads it to the draft release, and prints the publish command.
5. Review the listed artifacts and digests, then publish the draft release:

   ```sh
   gh release edit legion-vX.Y.Z --draft=false --latest=false
   ```

### Making and publishing a preview update

Preview builds let your own devices update directly from your fleet before a tagged release. They
use the exact same signed update-set rule as stable releases, with a separate preview Ed25519 key
that never enters the repository:

1. Generate a preview signing keypair once outside the checkout (private key mode `600`, never
   printed):

   ```sh
   node scripts/make-update-set.mjs keygen
   ```

   By default this creates `~/.legion-control/preview-update.key` and
   `~/.legion-control/preview-update.pub.pem`. Back them up alongside your Android release
   keystore.

2. Pick a strictly growing UTC sequence number (`YYYYMMDDHHMM`) and build the node and app with the
   preview public key compiled in:

   ```sh
   SEQ="$(node scripts/make-update-set.mjs sequence)"
   export LEGION_PREVIEW_UPDATE_KEY="$HOME/.legion-control/preview-update.pub.pem"
   export LEGION_BUILD_SEQUENCE="$SEQ"

   # Rebuild any fleet node that should store and propagate preview sets:
   cargo build --release --locked --bin legion

   # Build the Android APK with versionCode derived from $SEQ:
   ./apps/legion-app/build-android.sh --sequence "$SEQ"
   ```

   Official release builds leave `LEGION_PREVIEW_UPDATE_KEY` unset and never trust preview sets.

3. Assemble and sign the update set directory (`update.json`, artifacts, `SHA256SUMS`,
   `SHA256SUMS.sig`):

   ```sh
   REV="$(git rev-parse --short=12 HEAD)"
   node scripts/make-update-set.mjs create \
     --out "$HOME/.cache/legion-preview-$SEQ" \
     --channel preview \
     --version 0.1.0 \
     --build "$REV" \
     --sequence "$SEQ" \
     --notes "Faster refresh and bug fixes." \
     --key "$HOME/.legion-control/preview-update.key" \
     --artifact "apps/legion-app/src-tauri/gen/android/app/build/outputs/apk/universal/release/legion-release-arm64.apk:program=app,os=android,arch=aarch64,kind=apk"
   ```

4. Publish the signed set to any machine in the fleet:

   ```sh
   legion update.publish from="$HOME/.cache/legion-preview-$SEQ"
   ```

   The node verifies the signature and every artifact hash against its compiled-in trusted keys,
   stores the set under `state/updates/preview/<sequence>/` (keeping the newest two per channel),
   and notifies every reachable fleet peer with node authority so each peer pulls, verifies for
   itself, and stores the set. Release-only peers without `LEGION_PREVIEW_UPDATE_KEY` refuse the
   preview set cleanly and store nothing. Check what a node holds with `legion update.list`.
