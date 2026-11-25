# AI agents

This page is for both people connecting an AI coding agent to Legion and AI agents calling Legion
tools.

## The MCP door (`legion mcp`)

`legion mcp` serves the node's tool registry over the Model Context Protocol on stdin and stdout
(using the official `rmcp` SDK, which supports both revisions of the protocol: with and without the
`initialize` handshake). The same MCP door is also available over streamable HTTP at `/mcp` when
running `legion serve`.

### Compact mode vs full mode

`legion mcp` has two modes:

- **Compact mode (`legion mcp --as <principal> --compact`)**: exposes four meta-tools instead of
  the full registry:
  - `legion_tools` (`readOnlyHint: true`) — list the tools this principal may call, each with its
    effect (optional `family` filter such as `"fs"` or `"power"`).
  - `legion_describe` (`readOnlyHint: true`) — return one tool's description, effect, and input
    JSON Schema.
  - `legion_read` (`readOnlyHint: true`, `destructiveHint: false`, `idempotentHint: true`) — run
    an allowed tool whose effect is `read`, with optional `args` and optional `on`. Any other
    tool is refused without running, with the code `denied` and a hint to use `legion_call`.
  - `legion_call` (`readOnlyHint: false`, `destructiveHint: true`) — run any allowed tool by name,
    with optional `args` and optional `on`.
- **Full mode (`legion mcp --as <principal>`)**: exposes every allowed tool individually, replacing
  the dot with an underscore (`fs.read` becomes `fs_read`), adding an optional `on` string argument
  to every tool, and mapping effects to standard MCP annotations (`readOnlyHint: true` for `read`;
  `readOnlyHint: false, destructiveHint: true` for `write`, `exec`, `destroy`, `power`, and
  `admin`).

**Compact mode is the default when registering with `legion agents add`** because Legion ships more
than seventy built-in tools: loading every schema into every conversation consumes thousands of
tokens of context before the first user prompt. In compact mode the agent carries four small
schemas and fetches only the schemas it actually needs with `legion_describe`.

**Use `legion_read` for tools whose effect is `read`, and `legion_call` for every other.** Both
take the same arguments. `legion_call` can run anything, so it is annotated as destructive, and a
client that approves only read-only tools without asking stops at it: Codex in non-interactive
mode refuses it outright ("MCP tool call requires approval, but approval policy is never"), even
for `meta.whoami`. `legion_read` is annotated read-only because it keeps that promise: the door
looks up the tool's effect when it is called (from this node's spec, or for a tool that exists
only on the machine `on` names, from what that machine reports) and refuses anything that is not
`read`, or whose effect it cannot establish, before the call reaches a node. The refusal is the
door's own, like a malformed `on`, so it writes no audit line; a call it does run is authorised by
the policy and audited exactly as through `legion_call`. The annotation is a promise about the
tool, never a way past the policy. For an extension tool, `read` is what the machine's owner
declared in `tools.d`, so it is that owner's promise.

Over HTTP (`legion serve`), request compact mode with `/mcp?compact=1` or the header
`Legion-Compact: 1`.

Long-lived MCP sessions watch the registry dynamically, so extension tools added or removed in
`tools.d/` appear without restarting the session; full-mode sessions also emit
`notifications/tools/list_changed` when `tools.d/` changes.

## Registering agents with `legion agents`

`legion agents` registers, inspects, and removes the `legion` MCP server in four supported AI
coding agents by running each agent's own CLI command — it never edits an agent's configuration
files directly:

| Agent | CLI program | Registration command built by `legion agents add` |
| --- | --- | --- |
| `claude` | `claude` | `claude mcp add -s <scope> legion -- <legion> mcp --as <principal> [--compact]` |
| `codex` | `codex` | `codex mcp add legion -- <legion> mcp --as <principal> [--compact]` |
| `antigravity` | `agy` | `agy mcp add legion -- <legion> mcp --as <principal> [--compact]` |
| `opencode` | `opencode` | `opencode mcp add legion --<scope> -- <legion> mcp --as <principal> [--compact]` |

First add a principal to `policy.toml` (for example `[principals."agent:claude"]` with
`role = "editor"` or `role = "observer"` — see [Permissions](permissions.md)). Then preview or run
`legion agents add`:

```sh
legion agents add claude --as agent:claude --dry-run --json
```

```json
{
  "agent": "claude",
  "command": [
    "claude",
    "mcp",
    "add",
    "-s",
    "user",
    "legion",
    "--",
    "/home/me/.local/bin/legion",
    "mcp",
    "--as",
    "agent:claude",
    "--compact"
  ],
  "registered": false
}
```

Drop `--dry-run` to execute it. Common flags:

- `--full` (or `--no-compact`) — expose every tool individually instead of compact mode.
- `--scope <scope>` — for `claude` (`user` default, `local`, `project`) and `opencode` (`global`
  default, `project`).
- `--allow-owner` — required if you intentionally register with `--as owner`.
- `legion agents list` — check which agent CLIs are on `PATH` and whether `legion` is registered.
- `legion agents remove <agent>` — remove the registration through the agent's CLI.

## How an agent should work with Legion

1. **Start with `meta.whoami`.** Call `meta_whoami` (full mode) or
   `legion_read(tool="meta.whoami")` (compact mode) at the start of a task. It returns your
   `principal`, `role`, `node`, `paths` (if confined), and the exact list of `tools` you are
   allowed to call on this node.
2. **Discover before guessing.** In compact mode, call `legion_tools` (pass `family="fs"`,
   `family="job"`, etc. to stay concise) and then `legion_describe` for the tool you plan to call
   so your arguments match its strict schema (`additionalProperties: false`).
3. **Use `on` to target other machines.**
   - Omit `on` to run on the local node.
   - Pass a machine name (`"pi"`) to run on one remote machine (from `fleet.list`).
   - Pass comma-separated names (`"tower,pi"`), a JSON list (`["tower", "pi"]`), or `"all"` to run
     concurrently across multiple machines. Fan-out returns
     `{"results": {"<machine>": {"ok": ...} | {"error": ...}}}`. One failing machine never hides
     the results from the others.
   - Remember that each remote machine checks **its own** `policy.toml` for your principal.
4. **Branch on error `code` and read `hint`.** Failed tool calls return an MCP tool result with
   `isError: true` and the JSON error object as text:
   `{"code": "...", "message": "...", "hint": "...", "machine": "...", "details": {...}}`.
   Always inspect `code` and follow `hint` instead of retrying blindly.
5. **Run long work through `job.*`, not `exec.run`.** `exec.run` is for short, bounded commands
   (default timeout 60 s, max 3600 s) and dies if the call is cancelled. Anything long-running
   belongs in `job.start`:
   - Pass a deterministic `key` so retrying after a dropped connection returns the existing job
     (`existing: true`) instead of starting a duplicate.
   - Pass `exclusive: true` when unattended `power.*` actions must wait for the job to finish.
   - Wait with `job.wait` instead of tight-looping `job.status`.
   - Read output incrementally with `job.logs`, feeding each reply's `next_offset` back as `offset`.
   - Stop work cleanly with `job.cancel`.
6. **Never move bulk data through JSON.** `fs.read` caps each reply at 1 MiB and HTTP bodies cap at
   4 MiB. Move large files or directory trees with `fs.transfer` (`from` / `to` as `machine:/path`
   or local paths), which streams raw bytes over `legion stream`, resumes partial files, and verifies
   BLAKE3 hashes.
7. **Use safety rails.** Pass `expect_hash` to `fs.write` and `fs.edit` to avoid clobbering
   concurrent edits, and call destructive, sync, or power tools with `dry_run: true` first to
   inspect what will happen.
8. **Wake an off machine and wait for it.** When a call fails with `unreachable` and the machine
   has Wake-on-LAN configured (`fleet.list`), call `power.wake` with `wait_s` from a node that is
   on, check `awake: true`, and then continue:

   ```json
   {"tool": "power.wake", "args": {"machine": "tower", "wait_s": 180}}
   ```

   `power.wake` sends the wake packet through a helper and polls until the machine answers (or
   returns `awake: false` after `wait_s` seconds); only retry the original tool once `awake` is
   `true`.

## Error codes

Every door uses the same closed set of 13 error codes:

| Code | Meaning | What to do |
| --- | --- | --- |
| `invalid_args` | Arguments did not match the tool's schema or a value was invalid (for example a relative path). | Read the tool schema (`legion_describe` / `legion help <tool>`), fix the argument, and retry. |
| `unknown_tool` | No tool with that name exists on the target node. | Call `legion_tools` / `legion tools` on that machine to check spelling and installed extensions. |
| `not_found` | The named file, machine, job, project, service, token, or boot target does not exist. | Check the name (`fs.list`, `fleet.list`, `job.list`, `proj.list`, `svc.list`, `power.status`) and fix or create it. |
| `exists` | The resource already exists (for example `ext.define` without `replace=true` or duplicate token name). | Choose a new name, or pass the tool's explicit replace/overwrite flag if intended. |
| `conflict` | State changed since you last read it (`expect_hash` mismatch, `job.start` `key` reused with different arguments, `svc.*` name in both user and system scope, `fleet.sync` divergence, or a node name mismatch with `details.kind = "name_mismatch"`). | Re-read current state, disambiguate (`scope`), inspect the diff, or run `fleet.sync direction=push` to align a host-derived node name. |
| `denied` | Policy refused the principal, tool, path, or action on this node or a forwarding node. | Do not retry the same call. Check `meta.whoami`, adjust the path to an allowed root, or ask the owner to update `policy.toml`. |
| `unreachable` | The target machine could not be reached or its session dropped mid-call. | Check `fleet.ping` or wake the machine (`power.wake`). Only retry idempotent calls or calls with a `key`. |
| `timeout` | The command, probe, or wait exceeded its time limit. | Increase `timeout_s` if appropriate, or switch to `job.start` + `job.wait`. |
| `cancelled` | The caller or server cancelled the call before it finished. | Check whether the underlying state changed before retrying. |
| `busy` | A busy probe, an `exclusive` job, or an `unmonitored` node blocked a power action, or a lock/server concurrency limit was hit. | Run `power.status` to see why; wait for work to finish, configure `busy` in `node.toml`, or ask before passing `force=true`. |
| `unsupported` | This platform, node version, or protocol does not support the requested operation. | Read `hint` for the supported platforms or alternative tool. |
| `failed` | The operation was attempted and failed (non-zero command exit, broken config file, I/O error). | Read `message`, `hint`, and `details` (for example the line number in a TOML file) and address the root cause. |
| `internal` | An unexpected bug inside Legion itself. | Do not loop; report the message and context to the owner. |

## Adding an extension tool safely

If your principal has the `admin` effect, you can add reusable tools in `tools.d/` with
`ext.define`. Follow these rules:

1. **Pick an unreserved `family.verb` name.** Every family present in the built-in registry plus
   the reserved families (`agent`, `audit`, `config`, `exec`, `ext`, `fleet`, `fs`, `job`,
   `legion`, `meta`, `power`, `proj`, `svc`, `sys`, `token`) cannot be shadowed. Use a domain
   prefix such as `ops.*`, `gpu.*`, or `backup.*`.
2. **Declare `effect` honestly.** Policy and MCP confirmation hints rely on the declared effect.
   A command that writes files must be `write`; one that deletes must be `destroy`; one that runs
   arbitrary subcommands or mutates services must be `exec`. Never label a mutating command `read`.
3. **Prefer `run` (exact argv) over `cmd` (shell).** Avoid invoking a shell unless you need shell
   features.
4. **Never splice arguments into a command string.** Caller arguments are validated against `params`
   and passed as `LEGION_ARG_<NAME>` environment variables (plus a JSON object on stdin, alongside
   `LEGION_TOOL`, `LEGION_PRINCIPAL`, and `LEGION_NODE`). Quote `"$LEGION_ARG_..."` in shell
   commands and validate discrete choices with `enum`. String arguments larger than 32 KiB per
   variable (or 96 KiB total) are refused by default (`large = "refuse"`); set `large = "stdin"` on
   tools that read the full value from stdin JSON (the oversized variable is omitted and
   `LEGION_ARG_<NAME>_OMITTED=1` is set).
5. **Set a reasonable `timeout_s`** (default 60 s, max 3600 s) and test the new tool immediately
   with `legion_describe` / `legion help <name>` and a non-destructive call.
