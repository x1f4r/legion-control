# Legion

Legion is a single Rust binary, `legion`, installed on each machine you own. It gives people and
AI coding agents one uniform, self-describing set of tools for the machine it runs on and for every
other machine it can reach: system state, commands and long-running jobs, files and file transfer,
projects, services, power, and the fleet itself.

## The idea in five sentences

1. **Everything is a tool.** Every operation has a name (`fs.read`), a description, a JSON Schema
   for its arguments, and an *effect* (`read`, `write`, `exec`, `destroy`, `power`, `admin`).
   Nothing is reachable that is not a tool, and `legion tools` lists the entire registry.
2. **Every tool runs anywhere.** Any call accepts an optional target (`--on` on the CLI, `on` in
   MCP and HTTP): one machine, a list of machines, or `all`. The node you talk to runs it locally
   or forwards it, and the result has the same shape either way.
3. **A machine is anything that gives you a pipe to `legion`.** By default the pipe runs over
   `ssh`, using your existing SSH config and keys. It can also be any local command (`docker exec`,
   `wsl`, or a test harness). There is no daemon to keep alive and no port to open: if SSH works,
   Legion works.
4. **Three doors, one registry.** The command line, MCP (for AI agents), and an optional loopback
   HTTP server are thin adapters over the same tool registry. A built-in tool or a local extension
   in `tools.d/` appears in all three.
5. **Permissions are checked where the work happens.** Each node decides what a caller may do from
   its own `policy.toml`, records every call in `state/audit.jsonl`, and never trusts another node
   or caller to have checked first.

## What Legion is not

- **Not a central server or cloud service.** Every machine is a peer with its own configuration. No
  controller has to stay online for the others to work.
- **Not a container or OS sandbox.** A role with the `exec` effect can run commands as the OS
  account `legion` runs under; real filesystem boundaries require a role without `exec` behind a
  door that pins the principal (a restricted SSH key or an HTTP bearer token).
- **Not the older Node.js agent or GUI apps in this repository.** Those live beside the Rust
  workspace and keep their own files under `~/.legion-control` and `~/.config/legion-control`.
  Neither touches the other's configuration or state.

## Two-minute tour

Install `legion` on Linux or macOS (see [Getting started](getting-started.md) for Windows and
manual installs):

```sh
sh install/legion.sh
```

Initialize this machine's configuration (`node.toml` and `policy.toml`) without overwriting
anything that already exists:

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

List the tools built into the binary (71 tools across 14 families, plus any local extensions):

```sh
legion tools
```

Run three tools on this machine:

```sh
legion meta.version
```

```text
name      legion
version   0.1.0
protocol  1
node      tower
os        linux
arch      x86_64
```

```sh
legion fs.list path=~/docs
```

```text
path       /home/me/docs
entries
  - name   hello.txt
    path   /home/me/docs/hello.txt
    kind   file
    size   17
    mtime  2026-11-24T02:40:18.942Z
    mode   0644
truncated  false
```

```sh
legion exec.run argv='["uname","-s"]'
```

```text
Linux
exit_code         0
stdout            Linux
stderr            
stdout_truncated  false
stderr_truncated  false
timed_out         false
duration_ms       0
```

Add a second machine to `fleet.toml` (replace `pi` with any host or alias `ssh` accepts) and check
that it answers:

```sh
legion fleet.add name=pi ssh=pi
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
```

Run the same three tools on `pi` — or across both machines at once — with `--on`:

```sh
legion meta.version --on pi
legion fs.list path=~/docs --on pi
legion exec.run argv='["uname","-s"]' --on tower,pi
```

```text
[tower] Linux
[pi] Linux
pi
  exit_code         0
  stdout            Linux
  stderr            
  stdout_truncated  false
  stderr_truncated  false
  timed_out         false
  duration_ms       0

tower
  exit_code         0
  stdout            Linux
  stderr            
  stdout_truncated  false
  stderr_truncated  false
  timed_out         false
  duration_ms       0
```

Copy a file from this machine to `pi` with `legion cp`:

```sh
legion cp ~/docs/hello.txt pi:~/docs/copied.txt
```

```text
from             tower:/home/me/docs/hello.txt
to               pi:~/docs/copied.txt
kind             file
files_sent       1
bytes_sent       17
files_unchanged  0
deleted          0
conflicts        -
skipped          -
duration_ms      9
dry_run          false
```

Output is formatted for reading on a terminal and printed as compact JSON when piped to another
program; pass `--json` to get JSON anywhere.

## What is tested where

The unit and integration suites run on Linux, macOS, and Windows on every pull request; they reach
"another machine" through a local `command` connector with its own `LEGION_HOME`, so routing,
sessions, transfer, jobs, and policy are covered on each OS, but without ssh. The `Legion e2e`
workflow (`scripts/legion-e2e.mjs`) adds real ssh, one operating system at a time: each runner
reaches a second node on itself through a loopback sshd (on Windows once with cmd and once with
PowerShell as sshd's shell), so a Linux node driving a Windows one, or any other mix, is not
covered. That run installs with the install script and covers `fleet.install`, routed calls with
awkward arguments, file transfer, jobs started with an argument list and with a shell command that
outlive their session, policy and both audit logs across the hop, a key forced to `legion gate`,
services, MCP, and an extension tool; of power, only `power.status` and dry runs of
`power.restart` and `power.shutdown`; it does not cover `via` relays or the HTTP door. By hand,
Legion has run between real Linux and macOS machines, but the power tools only ever with
`dry_run`: a real sleep, restart, shutdown, or wake of a physical machine and switching a
dual-boot machine to its other system have never been tested, and neither has Legion between two
separate Windows machines or user accounts.

## Documentation map

| Page | What it covers |
| --- | --- |
| [Getting started](getting-started.md) | Installing on Linux, macOS, and Windows; first run; file locations (`LEGION_HOME`); adding machines, dual-boot systems, and `via` relays; troubleshooting real errors. |
| [Permissions](permissions.md) | Principals, roles, rules, effects, `paths`, protected secret locations, how each door pins a principal, audit logging and redaction, and worked `policy.toml` recipes. |
| [AI agents](agents.md) | `legion mcp` (compact and full mode), `legion agents add`, operating rules for agents, the 13 error codes, and adding extension tools safely. |
| [Guides](guides.md) | Task walk-throughs: file transfer, long jobs, projects, services, power and wake, the HTTP door, extension tools, configuration sync, and updates. |
| [Tool reference](tools.md) | Generated reference of every built-in tool, its effect, and its input schema. |
| [Design](design.md) | Architecture, workspace crates, wire and stream protocols, platform rules, and milestones. |
| [`examples/legion/`](../../examples/legion/) | Validated, commented example files for `node.toml`, `policy.toml`, `fleet.toml`, `projects.toml`, `tokens.toml`, and `tools.d/`. |
