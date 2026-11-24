# Getting started

This page walks through installing `legion` on Linux, macOS, and Windows, initializing your first
node, adding other machines, and reading the errors you are most likely to see.

## Install

Release binaries are published under `legion-v*` tags on GitHub alongside `SHA256SUMS`, an offline
Ed25519 signature (`SHA256SUMS.sig`, signed with the owner's offline key whose public key is
compiled into `legion` from `contract/release-public-key.pem`), and GitHub build provenance
attestations. Five targets are built in CI:

| Platform | Target triple | Asset name |
| --- | --- | --- |
| Linux x86-64 (static musl) | `x86_64-unknown-linux-musl` | `legion-<ver>-x86_64-unknown-linux-musl` |
| Linux arm64 (static musl) | `aarch64-unknown-linux-musl` | `legion-<ver>-aarch64-unknown-linux-musl` |
| macOS Apple Silicon | `aarch64-apple-darwin` | `legion-<ver>-aarch64-apple-darwin` |
| macOS Intel | `x86_64-apple-darwin` | `legion-<ver>-x86_64-apple-darwin` |
| Windows x86-64 (static CRT) | `x86_64-pc-windows-msvc` | `legion-<ver>-x86_64-pc-windows-msvc.exe` |

32-bit ARM Linux is not supported (use a 64-bit OS image on Raspberry Pi). Native Windows ARM64
binaries are not published yet; the Windows installer and `fleet.install` warn and install the
x86-64 binary to run under emulation (or build from source natively).

### Linux and macOS (installer script)

From a checkout of this repository, run the POSIX shell installer without `sudo`:

```sh
sh install/legion.sh
```

What `install/legion.sh` does:

- Detects the OS and CPU architecture (on macOS running under Rosetta 2, checks
  `sysctl.proc_translated` and selects the native `aarch64-apple-darwin` build), resolves the
  highest published stable `legion-v*` release, and downloads over HTTPS with TLS 1.2 or newer
  (`curl --proto '=https' --tlsv1.2`, or GNU `wget --https-only`; BusyBox `wget` is refused).
- Downloads the binary and `SHA256SUMS`, verifies the SHA-256 digest, and verifies release trust:
  1. If `gh` is on `PATH`, runs `gh attestation verify` against the release workflow and tag ref.
  2. Otherwise, if `openssl` supports Ed25519 `-rawin` (OpenSSL 3 / modern OpenSSL on Linux, or
     Homebrew OpenSSL 3 on macOS — macOS ships LibreSSL, which lacks `-rawin`), downloads
     `SHA256SUMS.sig` and verifies it against the pinned release public key embedded in the script.
  3. If neither `gh` nor an Ed25519-capable `openssl` is installed, the script stops without
     installing and prints how to install `gh` or OpenSSL 3 (for example `brew install openssl` on
     macOS), or pass `--insecure-skip-verify` to install with SHA-256 checking only.
- Refuses a directory destination, stages the binary beside `~/.local/bin/legion` with mode `0755`,
  atomically renames it into place, runs `~/.local/bin/legion --version`, and prints a reminder if
  `~/.local/bin` is not on your `PATH`.

Useful options:

```sh
sh install/legion.sh --version 0.1.0              # install a specific release
sh install/legion.sh --from target/release/legion # install a local binary offline
sh install/legion.sh --insecure-skip-verify       # skip signature/attestation (still checks SHA-256)
```

macOS behaviour was verified from code (`install/legion.sh`) rather than run here.

### Windows (installer script)

*(Not run here; documented from `install/legion.ps1`.)*

Run the PowerShell installer as your own user (no administrator rights required):

```powershell
.\install\legion.ps1
```

What `install/legion.ps1` does:

- Requires Windows PowerShell 5.1 or newer and forces TLS 1.2 on 5.1. On Windows ARM64, prints a
  warning and installs the `x86_64-pc-windows-msvc` binary to run under x64 emulation.
- Downloads the release binary and `SHA256SUMS`, verifies the SHA-256 checksum, and verifies trust
  in the same order as the Unix script: `gh attestation verify` if `gh` is on `PATH`, else
  `openssl pkeyutl -verify -rawin` (for example OpenSSL 3 from Git for Windows) against
  `SHA256SUMS.sig` and the pinned public key. If neither is installed, it stops and explains how to
  install `gh` or OpenSSL 3, or pass `-InsecureSkipVerify` (which still checks SHA-256).
- Installs to `%LOCALAPPDATA%\legion\legion.exe`. If an existing `legion.exe` is running, the
  script renames it aside to a `.old.<id>` file before moving the new binary in; `legion` removes
  stale `.old.*` files on its next start.
- Adds `%LOCALAPPDATA%\legion` to `HKCU\Environment\Path` if not already listed, preserving the
  `REG_EXPAND_SZ` value kind and existing unexpanded `%VAR%` entries (open a new terminal to call
  `legion` by name).

Options:

```powershell
.\install\legion.ps1 -Version 0.1.0
.\install\legion.ps1 -From target\release\legion.exe
.\install\legion.ps1 -InsecureSkipVerify
```

### Manual install or building from source

To install manually without a script:

1. Download the matching binary, `SHA256SUMS`, and `SHA256SUMS.sig` from the `legion-v<version>`
   release.
2. Verify `SHA256SUMS.sig` against `contract/release-public-key.pem` with OpenSSL 3 (`openssl
   pkeyutl -verify -pubin -inkey contract/release-public-key.pem -rawin -in SHA256SUMS -sigfile
   SHA256SUMS.sig`) or verify build provenance with `gh attestation verify`.
3. Verify the checksum (`sha256sum -c SHA256SUMS --ignore-missing` on Linux, `shasum -a 256` on
   macOS, `Get-FileHash -Algorithm SHA256` on Windows).
4. Move the binary to `~/.local/bin/legion` (`chmod 0755`) on Unix, or
   `%LOCALAPPDATA%\legion\legion.exe` on Windows, and make sure that directory is on `PATH`.

To build from source (requires the Rust toolchain pinned in `rust-toolchain.toml`):

```sh
cargo build --release --locked -p legion-cli
sh install/legion.sh --from target/release/legion
```

## First run

Run `legion init` on the machine you are sitting at:

```sh
legion init --name tower
```

```text
node  tower
created
  - /home/me/.config/legion/node.toml
  - /home/me/.config/legion/policy.toml
kept  -
next
  - Edit policy.toml to assign principals to a role (`observer`, `editor`, or `operator`) before connecting an AI agent or remote device.
  - Add your other machines to fleet.toml with `legion fleet.discover` (or `legion fleet.discover add=true`, or `legion fleet.add`).
  - Register an AI coding agent with `legion agents add claude --as agent:claude` (also supports codex, antigravity, and opencode).
  - Check that everything is configured and reachable with `legion meta.doctor`.
```

`legion init` is sugar over `meta.init`. It creates starter `node.toml` and `policy.toml` files
only when they do not already exist and never overwrites or reformats existing files:

```sh
legion init --name tower
```

```text
node     tower
created  -
kept
  - /home/me/.config/legion/node.toml
  - /home/me/.config/legion/policy.toml
```

Pass `--no-roles` to skip creating `policy.toml`, or `--dry-run` to preview without writing.

Next, run `legion meta.doctor` to check the node:

```sh
legion meta.doctor
```

`meta.doctor` checks that the configuration directory is readable, the state directory is writable,
every configuration file parses strictly, `tools.d/` is valid, the `legion` binary and `ssh` are on
`PATH`, systemd user lingering is enabled on Linux (so background jobs survive logout), policy
roles do not grant `exec` unexpectedly, and every machine in `fleet.toml` answers. Pass
`quick=true` to skip pinging remote machines.

## Where files live

| Purpose | Linux and macOS | Windows (not run here; from code) |
| --- | --- | --- |
| Configuration | `$XDG_CONFIG_HOME/legion` or `~/.config/legion` | `%APPDATA%\legion` |
| State | `$XDG_STATE_HOME/legion` or `~/.local/state/legion` | `%LOCALAPPDATA%\legion` |

Setting `LEGION_HOME` overrides both directories with `$LEGION_HOME/config` and
`$LEGION_HOME/state`. `LEGION_HOME` does not change what `~` expands to in tool paths.

Files inside the configuration directory (all strict TOML — unknown keys are rejected):

- `node.toml` — this machine's name, system name, busy probes, `[power]`, `[boot.<name>]`, `[svc]`,
  and `[serve]`. Machine-local; never synced.
- `fleet.toml` — other machines this node can reach.
- `policy.toml` — principals, roles, allow/deny rules, and path confinement.
- `projects.toml` — named projects, their per-machine directories, and tasks.
- `tokens.toml` — BLAKE3 hashes of bearer tokens for `legion serve`. Machine-local; never synced.
- `tools.d/*.toml` — user-defined extension tools.

Files inside the state directory:

- `audit.jsonl` (and `audit.lock`) — append-only JSONL audit log of every call.
- `jobs/<id>/` — records, logs, and locks for detached background jobs.
- `sync/sidecars/` and `sync/backups/` — BLAKE3 sync history and backups written by `fleet.sync`
  and `config.write`.
- `dist/<version>/<target>/` — verified release binaries cached by `meta.update` and
  `fleet.install`.

A missing configuration file means its empty default. A file that fails to parse fails the call
that needed it, naming the file and line number, rather than falling back to a default that might
widen access. An existing file that is empty (or comments only) is also refused for documents whose
empty default grants more than a written file would (`policy.toml` and `projects.toml`); `node.toml`,
`fleet.toml`, and `tokens.toml` allow empty files.

## Adding machines

### Discover peers from Tailscale

If your machines are on a Tailscale tailnet, `fleet.discover` queries `tailscale status --json` and
lists peer machines that belong to **your own** Tailscale user (`Self.UserID`):

```sh
legion fleet.discover
```

Shared-in devices belonging to someone else are skipped automatically so `--on all` can never reach
another person's machine. Tagged nodes are listed with `addable: false` and skipped by `add=true`
(add them explicitly with `fleet.add` if you want them).

To add every unlisted addable peer directly to `fleet.toml`:

```sh
legion fleet.discover add=true
```

### Add a machine directly

Use `fleet.add` to add or replace a machine entry atomically while keeping comments and unrelated
entries in `fleet.toml`:

```sh
legion fleet.add name=pi ssh=pi
```

```text
name   pi
added  true
```

`ssh` accepts any destination string your system `ssh` client understands (`pi`, `me@pi.example`,
an alias from `~/.ssh/config`, a Tailscale MagicDNS name). Legion never stores SSH keys and never
quotes caller arguments for a remote shell: it invokes fixed command shapes over `ssh` with
`BatchMode=yes`.

If `legion` is installed outside the remote `PATH` and outside `~/.local/bin/legion`, set `program`
in `fleet.toml` (for example `program = "/opt/legion/legion"` or `program = 'C:\Tools\legion.exe'`).
You can also set `os = "unix"` or `os = "windows"` to skip fallback probing on first connect.

A machine does not have to use SSH. A `command` entry runs any local argv that speaks `legion rpc`
on stdin/stdout (end the argv with `"rpc"` so `legion cp` can derive `"stream"`):

```toml
[machines.box]
command = ["docker", "exec", "-i", "box", "legion", "rpc"]
```

### Install Legion on another machine over SSH

If a machine is listed in `fleet.toml` with an `ssh` transport and does not have `legion` yet (or
needs an upgrade), install it remotely using only `ssh` and `scp`:

```sh
legion fleet.install machine=pi dry_run=true
legion fleet.install machine=pi
```

`fleet.install` probes the remote OS and CPU architecture (selecting native arm64 on macOS under
Rosetta 2, and x86-64 under emulation on Windows ARM64 with a note), downloads the release binary,
`SHA256SUMS`, and `SHA256SUMS.sig` (or reuses a cached copy under `state/dist/`, or copies a local
binary given with `from=/path/to/legion`), verifies the offline Ed25519 signature against the
compiled-in public key and checks SHA-256, checks the downgrade floor, checks the binary format
header, stages and runs `--version` on the remote host before saving a `.prev` backup and swapping
into place, and opens a new `legion rpc` session (restoring `.prev` automatically if verification
fails).

### Check reachability

```sh
legion fleet.ping
```

```text
results
  - machine     pi
    reachable   true
    node        pi
    version     0.1.0
    os          linux
    arch        x86_64
    latency_ms  2
  - machine    shed
    reachable  false
    error
      code     unreachable
      message  Could not open a legion session on shed: the connection closed before hello (exit status 255).
      hint     Remote stderr: ssh: connect to host unreachable.example port 22: Connection refused
      machine  shed
```

## Dual-boot machines (systems)

A machine with more than one operating system uses `[[machines.<name>.systems]]` in `fleet.toml`
instead of top-level `ssh` or `command`:

```toml
[machines.tower]
mac = "02:00:00:00:00:01"
wake_via = ["pi"]

[[machines.tower.systems]]
name = "linux"
ssh = "tower"
os = "unix"

[[machines.tower.systems]]
name = "windows"
ssh = "tower-win"
os = "windows"
```

On the machine itself, set `system` in each OS's `node.toml` (`system = "linux"` on Linux,
`system = "windows"` on Windows). When you run a call with `--on tower`, Legion remembers which
system answered last and tries it first; if it does not answer, it tries the other systems. The
system that is actually booted answers `hello` and reports its `system` name and `boot_id`.

## Reaching a machine through another (`via`)

When a machine (`shed`) is only reachable from another node (`pi`), declare `via = "pi"` in
`fleet.toml`:

```toml
[machines.pi]
ssh = "pi"

[machines.shed]
via = "pi"
```

A relayed entry sets `via` alone — do not combine `via` with `ssh`, `command`, `systems`,
`program`, or `os`. The relay node (`pi`) must have its own `[machines.shed]` entry in its
`fleet.toml`. When you run `legion meta.version --on shed`, your node forwards the call to `pi` with
`on = "shed"`, and `pi` forwards it to `shed`. Every node along the path checks its own
`policy.toml` and writes to its own audit log.

## Troubleshooting

These transcripts come from the built binary with placeholder names.

### Unknown machine

```sh
legion meta.version --on ghost
```

```text
legion: not_found: There is no machine called ghost in fleet.toml on tower.
  Known machines in fleet.toml: pi (this node is tower).
```

Check the name with `legion fleet.list` or add the machine with `legion fleet.add`.

### Unreachable machine

```sh
legion meta.version --on ghost
```

```text
legion: unreachable: Could not open a legion session on ghost: the connection closed before hello (exit status 255).
  Remote stderr: ssh: connect to host unreachable.example port 22: Connection refused
```

Verify that plain `ssh -T -o BatchMode=yes <target> legion --version` works from the same account.
Common causes: the machine is asleep (wake it with `legion power.wake`), your SSH key needs an
agent or passphrase (`BatchMode=yes` disables interactive prompts), or `legion` is not on the
remote `PATH` or `~/.local/bin/legion` (set `program` in `fleet.toml` or run `fleet.install`).

### Denied call

```sh
legion fs.read path=~/docs/hello.txt --as unknown-bot
```

```text
legion: denied: unknown-bot may not use fs.read: policy.toml does not name unknown-bot, so it has the role nobody, which allows nothing.
  Add [principals."unknown-bot"] with a role to policy.toml, or call without --as.
```

Unknown principals always receive the built-in `nobody` role. Run `legion meta.whoami --as <name>`
on the target node to inspect the resolved role, path roots, and allowed tools.

### Unmonitored or busy machine

`power.*` calls with `dry_run=true` still return the planned commands and add a `would_refuse` block
showing the exact refusal a real call would return. On a fresh node whose `node.toml` does not
declare busy probes or `busy = "none"`:

```sh
legion power.sleep dry_run=true
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
  message  Refused to put this machine to sleep: node.toml does not say what keeps this machine busy, and not knowing is not the same as idle.
  hint     If nothing on this machine needs protecting, add the line busy = "none" to /home/me/.config/legion/node.toml. Otherwise describe what does with [[busy.probes]] tables (name, run = [program and arguments, exit 0 while busy and 1 while idle], timeout_s). To go ahead this once anyway, pass force: true.
```

When a busy probe reports active work (or an `exclusive` job is running):

```sh
legion power.sleep dry_run=true
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
  message  Refused to put this machine to sleep: the machine is busy (probe render: busy (the probe exited 0)).
  hint     Wait until what keeps it busy has finished (power.status shows the state), or pass force: true to go ahead anyway.
```

Run `legion power.status` to see which probes or exclusive jobs are active, or pass `force=true` to
override the busy gate once (on Linux, logind inhibitors are separate and require
`ignore_inhibitors=true` with `[power] sudo = true`).
