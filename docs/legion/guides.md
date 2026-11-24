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

A phone, tablet, or other controller pairs with a node once and afterwards connects over SSH,
confined to one principal by `legion gate --as <principal>`.

First make sure the principal exists in `policy.toml` with the role you want (for example
`[principals.phone]` with `role = "observer"`), then run `legion pair`:

```sh
legion pair --as phone
```

`legion pair` calls `device.invite`, prints who the device will act as and when the invite expires,
draws the invite as a terminal QR code, prints the invite text on one line (for copying and pasting
when a camera is inconvenient), and prints the commands to list and remove devices. Pass `--invert`
if your light-background terminal needs inverted QR polarity, `--valid 1h` (up to `24h`) to change
the 15-minute default, `--host`, `--port`, or `--user` to override detected connection details, or
`--json` for the raw `device.invite` output.

What happens during pairing:

1. `device.invite` generates a fresh single-use Ed25519 key pair and appends one line to
   `authorized_keys` carrying `restrict`, a portable 12-digit `expiry-time` upper bound (compatible
   with OpenSSH 8.9+), and `command="<legion> gate --enrol <id>"` (pinning `--home` when
   `LEGION_HOME` is non-default and `--account` on Windows's shared
   `administrators_authorized_keys`). The invite carries the node's detected Tailscale MagicDNS
   name and Tailscale addresses (or the system hostname when Tailscale is absent), the account and
   port, the node's sshd public host keys (pinned — scanned from local `sshd` via `ssh-keyscan`
   first, falling back to host `.pub` files, with `host_keys_source` noting where they came from
   and whether scanned keys differed from the files; invites without a usable host key are
   refused), and the temporary key. Neither the invite text nor the temporary key is logged or
   audited.
   **Keep the invite secret until used:** `device.invite` returns the invite text (which carries
   the temporary private key seed) in its tool result, so invoking `device.invite` through an AI
   agent puts the live invite into that agent's conversation transcript. Run `legion pair` directly
   in your own terminal whenever you do not want an agent transcript to see the invite.
2. The device scans the QR code or pastes the invite, connects over SSH verifying the pinned host
   key, and sends its own newly generated Ed25519 public key and name.
3. `legion gate --enrol <id>` validates the invite and public key under an owner-only lock file
   beside `authorized_keys`, enforces exact UTC expiry from `state/devices.json` (an expired
   temporary key stops working at its exact expiry because `legion gate --enrol` refuses it and
   the expired line is swept on the next `device.*` command), refuses enrolling the invite's own
   temporary key or any key already present in `authorized_keys`, replaces the temporary line with
   a permanent `restrict,command="<legion> gate --as phone --device <name>"` line, records the
   device in `state/devices.json`, and writes an audit record (`door: "ssh"`,
   `tool: "device.enrol"`). The temporary key immediately stops working.

Inspect paired devices and pending invites, or revoke one at once (removing a paired device also
terminates any active sessions for that device immediately and reports `sessions_ended`):

```sh
legion device.list
legion device.remove name=my-phone
legion device.remove id=<invite-id>
```

### Overriding `authorized_keys` and host keys (`[devices]`)

By default Legion edits `~/.ssh/authorized_keys` (on Windows, when the account is in the
`Administrators` group and `sshd_config` uses `administrators_authorized_keys`, it edits
`%ProgramData%\ssh\administrators_authorized_keys`, sets its owner to `BUILTIN\Administrators`
alongside the `Administrators` and `SYSTEM` ACL, pins `--account` on forced commands so each
administrator sees and manages only their own entries, and quotes forced commands for the shell
configured in `HKLM\SOFTWARE\OpenSSH\DefaultShell` — `cmd.exe` by default or PowerShell when set).
Host keys prefer `ssh-keyscan` against local `sshd` on the invite's port, falling back to
`/etc/ssh/ssh_host_*_key.pub` (or `%ProgramData%\ssh\ssh_host_*_key.pub` on Windows). To override
either for a custom sshd setup, set `[devices]` in `node.toml`:

```toml
[devices]
authorized_keys = "/home/me/.ssh/authorized_keys"
host_keys = ["/etc/ssh/ssh_host_ed25519_key.pub"]
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

- **Shared documents**: `fleet.toml`, `policy.toml`, `projects.toml`, and `tools.d/<name>.toml`.
  Extension files may contain internal dots (matching `ext.define` files such as
  `tools.d/ops.greet.toml`); on write, the receiver enforces the same rules as `ext.define` — the
  filename stem must match the `name` field inside the file, duplicate tool names across files are
  refused, and reserved families are rejected.
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
