# Legion: the everything API

One small Rust binary, `legion`, on every machine you own. It gives people and AI agents one
uniform, self-describing set of tools for the machine it runs on and for every other machine it
can reach: system state, commands and long-running jobs, files and file transfer, projects,
services, power, and the fleet itself.

This page is the design the Rust workspace is built to. It replaces the Node agent and the
contract in this repository; until it reaches their feature set the two live side by side and
neither touches the other's files.

## The idea in five sentences

1. **Everything is a tool.** A tool has a name (`fs.read`), a description, a JSON Schema for its
   arguments, and an *effect* (`read`, `write`, `exec`, `destroy`, `power`, `admin`). Nothing is
   reachable that is not a tool, and every tool is listed by the binary itself.
2. **Every tool runs anywhere.** A call carries an optional `on` — a machine, a list of machines
   or `all`. The node you talk to runs it locally or forwards it; the answer has the same shape
   either way.
3. **A machine is anything that gives you a pipe to a `legion` process.** By default the pipe is
   `ssh <host> legion rpc`. It can be any command (`docker exec`, `wsl`, a test harness). There is
   no daemon to keep alive and no port to open: if ssh works, Legion works.
4. **Three doors, one registry.** The command line, MCP (for AI agents) and an optional HTTP
   endpoint are thin adapters over the same tool registry. A tool added once appears in all three.
5. **Permissions are checked where the work happens.** Each node decides what a caller may do from
   its own policy file, writes every call to an audit log, and never trusts a caller to have
   checked first.

## Vocabulary

| Word | Meaning |
| --- | --- |
| node | a running `legion` process that owns a tool registry on one machine |
| machine | a named entry in the fleet file; one or more *systems* (a dual boot machine has several) |
| system | one operating system of a machine, with its own way of being reached |
| tool | a named operation with a schema and an effect |
| principal | who is calling: `owner`, `agent:claude`, `agent:gemini`, `phone`, … |
| role | a named set of allow and deny rules a principal is given |
| door | how a call arrives: `cli`, `mcp`, `http`, `rpc` |
| job | a command that outlives the call that started it |

## Workspace

    Cargo.toml                 workspace, shared dependency versions and lints
    crates/
      legion-proto/            wire types only: requests, replies, errors, tool specs (serde)
      legion-os/               the one place platforms differ: shells, process trees, detaching,
                               process identity, atomic files, locks
      legion-xfer/             the transfer engine: hashes, tree listings, plans, tar streams,
                               resumable single files; knows nothing about machines
      legion-core/             node: registry, context, policy, audit, config, paths
      legion-tools/            the built-in tool families, one module per family
      legion-net/              reaching other nodes: connectors, sessions, router, transfer
      legion-doors/            MCP (stdio and HTTP) and the JSON HTTP API
      legion-cli/              the `legion` binary: argument parsing and wiring

Dependencies point one way: `proto, os, xfer ← core ← tools ← net ← doors ← cli`. `legion-os`
and `legion-xfer` know nothing about tools or policy, and `legion-tools` never knows a call came
from another machine; `legion-net` wraps a local registry and adds the tools that only make sense
across machines.

Never name a source directory `bin`: the repository ignores `**/bin/`.

## Wire protocol

Newline-delimited JSON-RPC 2.0 over any byte stream. `legion rpc` speaks it on stdin and stdout;
stderr is for human-readable diagnostics only and is never parsed.

| Method | Params | Result |
| --- | --- | --- |
| `hello` | `{client, version}` | `{node, version, protocol, os, arch, systems}` |
| `tools` | `{}` | `{tools: [ToolSpec]}` |
| `call` | `{tool, args, as?, on?}` | the tool's result object |
| `cancel` | `{id}` | `{}` |

While a call runs the node may send notifications, `{"method":"event","params":{"id":<call id>,
"kind":"stdout"|"stderr"|"progress"|"log","data":…}}`. A client that does not care ignores them.

`protocol` is an integer, starting at 1. A node refuses a `hello` whose protocol it does not
speak with the `unsupported` error and says which it does. Additive changes (new tools, new
optional fields) do not bump it.

### Results and errors

A successful call returns the tool's own JSON object, never a bare scalar. A failure is a JSON-RPC
error whose `data` is:

    {"code": "denied", "message": "…", "hint": "…", "machine": "tower", "details": {…}}

`code` is one of a closed set, the same in every door, so an agent can branch on it:

`invalid_args`, `unknown_tool`, `not_found`, `exists`, `conflict`, `denied`, `unreachable`,
`timeout`, `cancelled`, `busy`, `unsupported`, `failed`, `internal`.

`message` says what happened in one sentence. `hint`, when present, says what to do about it.
Messages and hints are written for someone who has never seen the source.

### Fan-out

`on` naming more than one machine returns `{"results": {"<machine>": {"ok": …} | {"error": …}}}`.
Machines are called concurrently and one failing never fails the others.

### Bulk data

JSON carries control, never bulk bytes. File content beyond what `fs.read` allows in one reply
(1 MiB) moves over a second pipe, `legion stream`. Its first line on stdin is one JSON object
saying what to do; raw bytes follow in the direction the operation needs:

| `op` | Header | Then |
| --- | --- | --- |
| `get` | `{path, offset?}` | one JSON line `{size, hash?}` on stdout, then the file's bytes |
| `put` | `{path, size, hash, offset?, mode?}` | bytes on stdin; one JSON line on stdout when verified and renamed |
| `pack` | `{root, names: […]}` | a tar stream of those names on stdout |
| `unpack` | `{root}` | a tar stream on stdin; one JSON line on stdout when done |

They are the same permission-checked operations as the `fs.*` tools. Hashes are BLAKE3, written
`blake3:<hex>`.

The remote command line is therefore always one of two fixed strings, `legion rpc` or
`legion stream`, with every path and value inside JSON. Nothing a caller supplies is ever quoted
for a remote shell, which is the only way to be right about `sh`, `fish`, `cmd` and PowerShell at
once. A reader of either pipe skips lines that are not JSON before the first reply, so a login
banner or a shell's own noise cannot break a session.

## Tools

Every tool takes one JSON object and returns one JSON object. Arguments are validated against the
schema before the tool runs. Paths accept `~`; relative paths are refused. Sizes are bytes, times
are RFC 3339 UTC, durations are seconds.

| Family | Tools | Notes |
| --- | --- | --- |
| `meta` | `meta.version`, `meta.whoami`, `meta.doctor` | who am I, what may I do, is this node healthy |
| `sys` | `sys.info`, `sys.procs`, `sys.kill` | OS, CPU, memory, disks, GPU, uptime, busy |
| `exec` | `exec.run` | bounded command: timeout, captured output, kills the whole tree |
| `job` | `job.start`, `job.list`, `job.status`, `job.logs`, `job.cancel`, `job.wait` | outlives the session |
| `fs` | `fs.list`, `fs.stat`, `fs.read`, `fs.write`, `fs.edit`, `fs.mkdir`, `fs.move`, `fs.copy`, `fs.remove`, `fs.find`, `fs.grep`, `fs.hash` | one machine |
| `fs` (fleet) | `fs.transfer` | between any two machines, or to and from this one |
| `proj` | `proj.list`, `proj.add`, `proj.remove`, `proj.status`, `proj.clone`, `proj.pull`, `proj.run`, `proj.discover` | named projects and where they live |
| `svc` | `svc.list`, `svc.status`, `svc.start`, `svc.stop`, `svc.restart`, `svc.logs` | systemd, launchd, Windows services |
| `power` | `power.sleep`, `power.reboot`, `power.shutdown`, `power.targets`, `power.wake` | busy-gated; `reboot` can name the system to land in |
| `fleet` | `fleet.list`, `fleet.ping`, `fleet.add`, `fleet.remove`, `fleet.discover`, `fleet.install` | the machines and whether they answer |
| `ext` | `ext.list`, `ext.define`, `ext.remove` | tools you add yourself |
| `audit` | `audit.tail` | what was called, by whom |

Rules that hold for every tool:

- **Read tools never change anything**, including access times they can avoid touching.
- **Writes are atomic**: written beside the target, flushed, renamed over it. `fs.write` and
  `fs.edit` accept `expect_hash`; a file that changed since it was read fails with `conflict`
  instead of being overwritten.
- **Output is bounded.** Anything that can be large is paged or truncated, and the reply says so
  (`truncated: true`, `next`), so a reply never floods an agent's context.
- **Nothing is interpolated into a shell.** `exec.run` takes either `argv` (run exactly) or `cmd`
  (handed whole to the platform shell). Values a tool builds a command from are passed as
  arguments or environment, never spliced into a string.
- **A started thing is written down before it starts.** A job has a record with an id before its
  process exists, so a dropped link, a crash or a power cut leaves something to report on.
- **A destructive tool says what it will destroy.** `fs.remove` and friends accept `dry_run`.

### Jobs

`job.start` writes the job's record, then re-executes the binary as a detached supervisor. The
supervisor runs the command, appends its output to a log file and writes the exit record. State
lives under `state/jobs/<id>/`.

- Detaching is per platform and has to survive the session that asked: a transient
  `systemd-run --user --scope` where a user manager is running (logind kills what is left in an
  ssh session's scope), `setsid` otherwise; on Windows a process created outside the caller's job
  object, because sshd's job takes its children with it.
- A process is identified by its id *and* its start time, and a record also carries the boot id.
  A recycled process id never makes a dead job look alive.
- A job whose supervisor is gone without an exit record is reported as `interrupted`, naming how
  far it got. Reading a job's state never writes.
- `job.cancel` stops the whole process tree, and reports `cancelled` only once it is gone.
- `job.start` accepts a caller-chosen `key`. Starting again with the same key and the same
  command returns the existing job instead of a second one, so a retry after a dropped link is
  safe.

### Busy

A node is *busy* when a configured probe says so or a job marked `exclusive` is running. `power.*`
refuses with `busy` unless called with `force`, and says what is keeping the machine busy. A node
whose `node.toml` configures no probes reports `unmonitored`, which blocks unattended power
changes until the file says `busy = "none"` in so many words: not knowing is not the same as idle.

### Transfer

`fs.transfer {from, to}` where each side is `machine:/path` or a local path.

- One file: resumable. The destination keeps a partial file, the transfer continues from its
  length, and the result is renamed into place only after the whole-file hash matches.
- A directory: both sides list their tree (size and modification time, or hash with
  `checksum: true`), only what differs is packed, and a second run sends nothing. `delete: true`
  removes what the source no longer has.
- The bytes are relayed through the calling node, which can reach both ends by definition. A
  direct path between the two ends is an optimisation for later, not a different result.

### Extension tools

A file in `tools.d/` defines a tool: name, description, effect, parameter schema and a command.
Arguments reach the command as `LEGION_ARG_<NAME>` environment variables. Its standard output is
the result: parsed as JSON when it is JSON, otherwise returned as text. An extension cannot take
the name of a built-in tool, and defining one needs the `admin` effect.

## Reaching other machines

`fleet.toml` names the machines:

    [machines.tower]
    mac = "02:00:00:00:00:01"
    wake_via = ["pi"]

    [[machines.tower.systems]]
    name = "linux"
    ssh = "tower"

    [[machines.tower.systems]]
    name = "windows"
    ssh = "tower-win"

    [machines.pi]
    ssh = "pi"                      # one system, the short way

    [machines.box]
    command = ["docker", "exec", "-i", "box", "legion", "rpc"]

`ssh` is anything the system ssh client accepts, so aliases, jump hosts, agents and Tailscale all
keep working and Legion stores no key. A command run over ssh gets a bare `PATH` that rarely
holds a user's own programs, so the node asks for `legion` and, failing that, the place the
installer puts it; a system's `program` setting names it outright when it lives elsewhere. For a machine with several systems the one that answered
last is tried first; only the running system answers, and its `hello` says which it is.

The node keeps one session per machine open while it lives and multiplexes calls over it by id,
so only the first call to a machine pays for the handshake. A session that dies is reopened once
before the call fails with `unreachable`.

Waking is honest. `power.wake` asks a helper on the target's network to send the packet; the
helper takes the hardware address from its own fleet file, never from the request. The reply says
`sent`, and says `awake` only once the target itself answered. A sleeping helper is never woken in
order to wake something else.

Rebooting into another system is verified. The node arms the firmware for one boot with the
commands `node.toml` gives for that target, reads the setting back, and only then schedules the
restart a few seconds out so the reply still arrives. A machine counts as rebooted when it answers
with a different boot id and names the expected system, not when the link drops.

## Permissions

`policy.toml` on each node:

    default = "owner"               # the principal of a caller who names none

    [principals."agent:claude"]
    role = "operator"

    [roles.observer]
    allow = ["effect:read"]

    [roles.operator]
    allow = ["effect:read", "effect:write", "effect:exec"]
    deny  = ["fs.remove", "power.*"]
    paths = ["~/projects/**"]       # fs tools are confined to these roots

A rule is a tool name, a glob over tool names, or `effect:<effect>`. Deny wins over allow, and a
principal the node has never heard of gets the built-in role `nobody`, which allows nothing. With
no policy file at all, the only principal is `owner`, allowed everything: the default install
behaves like the shell it was started from.

How far a principal can be trusted depends on the door, and the design says so plainly:

- **A local shell is the owner.** `--as agent:claude` narrows what a cooperating agent can do and
  what the audit log records. It is a guard rail, not a wall: a process that can run the binary
  can run anything else the account can.
- **An ssh key can be a wall.** A key whose `authorized_keys` line forces
  `legion gate --as <principal>` can do exactly what that principal's role allows on that node.
  `gate` reads the command the client asked for without a shell and accepts exactly `legion rpc`
  and `legion stream`; anything else is refused.
- **An HTTP token is a wall.** Each token maps to one principal.

A door that pinned a principal ignores a different `as` in the request. Only a role with
`delegate = true` (the owner's) may act as someone else. A forwarded call names its principal to
the target, and the target applies its own policy to it.

Every call is appended to `state/audit.jsonl`: time, principal, door, tool, target, outcome and
duration. Arguments are recorded with file contents and anything that looks like a secret
replaced by their size.

## Doors

- **CLI.** `legion <tool> [key=value …] [--on M] [--as P] [--json]`. A value that parses as JSON
  is JSON, anything else is a string. `legion tools` lists them; `legion help <tool>` prints the
  schema. Output is a readable rendering on a terminal and the raw JSON object otherwise.
- **MCP.** `legion mcp` serves the registry over stdio. Tool names swap the dot for an underscore
  (`fs_read`), every tool gains the optional `on` argument, and effects become the standard
  annotations (`readOnlyHint` for `read`, `destructiveHint` for `destroy` and `power`). A failed
  call is a result with `isError` and the same error object as text, so the agent can read the
  code and the hint. `--compact` serves three tools instead (`legion_tools`, `legion_describe`,
  `legion_call`) for agents that should not carry forty schemas in their context. Two revisions
  of the protocol are in use, one with an `initialize` handshake and one without; the official
  SDK (`rmcp`) speaks both, so the door is built on it rather than written by hand.
- **HTTP.** `legion serve` listens on `127.0.0.1` only: `GET /v1/tools`, `POST /v1/call`, and MCP
  at `/mcp`. Every request needs a bearer token from `tokens.toml`, where only the token's hash
  is stored, and a request with an `Origin` header that is not this server is refused. Publishing
  it beyond the machine is Tailscale Serve's job, never a wider bind address.
- **Agents.** `legion agents add <claude|codex|antigravity|opencode>` registers the stdio door
  with that tool through the tool's own `mcp add` command, pinned to the principal you name.
- **RPC.** `legion rpc`, the pipe other nodes use.

## Files on disk

| What | Unix | Windows |
| --- | --- | --- |
| configuration | `$XDG_CONFIG_HOME/legion` or `~/.config/legion` | `%APPDATA%\legion` |
| state | `$XDG_STATE_HOME/legion` or `~/.local/state/legion` | `%LOCALAPPDATA%\legion` |

`LEGION_HOME`, when set, replaces both with `$LEGION_HOME/config` and `$LEGION_HOME/state`. Every
test runs under its own.

Configuration is TOML: `node.toml` (this node's name, boot targets, busy probes), `fleet.toml`,
`projects.toml`, `policy.toml`, `tokens.toml`, `tools.d/*.toml`. A missing file means its empty
default; a file that does not parse fails the one call that needed it with the line it failed on,
and never falls back silently to a default that would widen access.

## Platforms

Linux, macOS and Windows on x86-64 and arm64 are first class: every tool either works or fails
with `unsupported` and a hint, and CI runs the test suite on all three. Platform differences live
behind one module per concern (`shell`, `proctree`, `detach`, `services`, `power`), never as `cfg`
scattered through tool code.

Traps the Node agent already paid for, which the Rust code handles from the start:

- Replacing a file on Windows fails while an indexer or scanner holds it; a rename is retried
  with a short backoff for a few seconds, never replaced by delete-then-write.
- Locks are the kernel's (`flock`, `LockFileEx`), are never broken by age, and are released by
  the process dying. One node-wide lock orders everything that changes the machine.
- Text parsed from system tools is asked for in the `C` locale.
- Windows PowerShell is started with `-NoProfile -NonInteractive` and without an inherited
  `PSModulePath`.
- User services on macOS live in the `gui/<uid>` domain.

## Testing

- Each crate tests its own rules.
- Routing, sessions, fan-out and transfer are tested against a second node started through the
  `command` connector with its own `LEGION_HOME`: the same code path as ssh without needing an
  ssh server.
- Nothing in a test reads or writes outside its temporary directory, the network, or the real
  configuration of the machine it runs on.

## Milestones

1. **Foundation**: proto, core, CLI, `meta` tools, CI on three platforms.
2. **One machine**: `sys`, `exec`, `fs`, MCP over stdio.
3. **The fleet**: sessions, routing, fan-out, `fleet`, `job`, transfer.
4. **The rest**: `proj`, `svc`, `power`, `ext`, HTTP, tokens.
5. **Shipping**: install and update, release builds, the handbook.
