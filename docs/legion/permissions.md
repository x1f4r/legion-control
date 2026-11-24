# Permissions

Each Legion node enforces its own access rules from `policy.toml` in its configuration directory
and records every call in `state/audit.jsonl`.

## The central rule

**A role that may run commands (`exec`) is as strong as the operating system account.** A command
can read, write, and start anything the OS lets that user touch — including Legion's own
`policy.toml`, `tokens.toml`, and `audit.jsonl`. `paths` does not contain `exec`; it only chooses
where a command starts.

**Real boundaries require both:**

1. A role **without** the `exec` effect (such as `observer` or `editor`), and
2. A door that **pins** the principal and cannot be bypassed by the caller: a restricted SSH key
   forced through `legion gate --as <principal>`, or an HTTP bearer token served by `legion serve`.

A local shell running `legion --as <principal>` or a local AI agent started with
`legion mcp --as <principal>` is a **guard rail and an audit trail**, not a security wall: any
process running as your OS user already has the OS account's permissions outside Legion.

## Principals, roles, and rules

`policy.toml` maps **principals** (who is calling) to **roles** (what they may do):

```toml
default = "owner"

[roles.editor]
allow = ["effect:read", "effect:write"]
deny  = ["audit.*"]
paths = ["~/projects/**"]

[principals."agent:claude"]
role = "editor"
```

- **Principals** are names made of ASCII letters, digits, and `: . _ @ -` (up to 128 characters),
  such as `owner`, `phone`, `agent:claude`, or `script:backup`.
- **`default`** sets the principal for callers that do not pass `--as` on an unpinned door. When
  absent (or when `policy.toml` does not exist), `default` is `owner`.
- **Two roles are built in** and cannot be redefined in `policy.toml`:
  - `owner`: allows `*`, unconfined paths, `delegate = true`. `[principals.owner]` is also built in
    and cannot be redefined.
  - `nobody`: allows nothing. Any principal not named in `policy.toml` gets `nobody`.
- **Rules** in `allow` and `deny` are one of:
  - an exact tool name (`fs.read`, `power.status`),
  - a glob over tool names (`fs.*`, `power.*`, `*`), or
  - `effect:<effect>` (`effect:read`, `effect:write`, `effect:exec`, `effect:destroy`,
    `effect:power`, `effect:admin`).
- **Deny wins over allow.** A tool must match at least one `allow` rule and zero `deny` rules.
- **No silent fallbacks.** If `policy.toml` has a syntax error, an unknown key, or is an empty file,
  every call fails naming the file and line instead of falling back to `owner`.

## Effects and what each promises

Every tool declares one of six effects:

| Effect | Examples | What it promises (and does not promise) |
| --- | --- | --- |
| `read` | `meta.whoami`, `fs.read`, `sys.info`, `svc.status`, `power.status`, `audit.tail` | Inspects state without changing files, processes, services, or power state. |
| `write` | `fs.write`, `fs.edit`, `fs.copy`, `fs.move`, `fs.mkdir`, `fs.transfer`, `proj.add` | Creates or modifies files and project entries. **`write` can replace content**: denying `fs.remove` prevents deleting files, not overwriting their bytes with something else. |
| `destroy` | `fs.remove`, `sys.kill` | Removes files or directories or terminates processes. Replacing a whole directory counts as destroying it. |
| `exec` | `exec.run`, `job.start`, `job.cancel`, `svc.start`, `svc.stop`, `svc.restart`, `proj.run`, `proj.clone`, `proj.pull` | Runs programs on the machine. **`exec` is as strong as the account**: `paths` does not contain what a command reads, writes, or launches. |
| `power` | `power.sleep`, `power.restart`, `power.shutdown`, `power.wol`, `power.wake`, `power.switch` | Changes machine power state or sends Wake-on-LAN packets. |
| `admin` | `meta.init`, `meta.update`, `fleet.add`, `fleet.remove`, `fleet.install`, `fleet.sync`, `config.*`, `token.*`, `ext.define`, `ext.remove`, `agent.*` | Changes Legion itself: policy, tokens, fleet membership, extensions, synced config, agent registrations, and the binary. |

### Legion's own files belong to `admin`

No file tool (`fs.*` or `legion stream`) may read or write the node's configuration directory or
state directory for a role without `effect:admin`, regardless of what `paths` says. A role that can
write files cannot edit `policy.toml`, inspect `tokens.toml`, or tamper with `audit.jsonl` through
Legion's file tools.

## `paths` and protected locations

Setting `paths` on a role confines all file tools (`fs.*` and `legion stream`) to those directory
roots:

```toml
paths = ["~/projects/**"]
```

- Each entry must be an absolute path or start with `~`. A trailing `/**` is optional and accepted.
  Other wildcards (`*`, `?`, `[`) inside a path root are rejected.
- Every path passed to a file tool must be absolute or start with `~`; relative paths and Windows
  alternate data streams (`file:stream`) are refused.
- Directories along the path are resolved link by link, including `..` and symbolic links. A
  symbolic link inside an allowed root whose target points outside the root is refused, and a
  dangling link inside a confined role is refused.
- Whole-tree destructive actions (`fs.remove`, replacing a directory) always refuse the filesystem
  root and the user's home directory for every role, including `owner`, and refuse directories
  containing Legion's config or state for roles without `effect:admin`.

### Built-in secret locations

Even when a role has broad or unconfined `paths`, Legion refuses file-tool access to a built-in
list of well-known secret locations — SSH keys, GPG keyrings, cloud and git credentials, password
stores, AI agents' credential files, and browser profiles — for every role that does not allow
`effect:admin`, unless that role's `paths` explicitly names the location.

## The three ready-made roles

`legion init` writes a starter `policy.toml` with `default = "owner"` and three roles:

- **`observer`**: `allow = ["effect:read"]`, `deny = ["audit.*"]`, `paths = ["~/projects/**"]`.
  May look without changing anything (system facts, job/service/project status, and files under
  `paths`, never Legion's audit log). A real boundary when paired with a pinned door.
- **`editor`**: `allow = ["effect:read", "effect:write"]`, `paths = ["~/projects/**"]`. May read
  and modify files inside `paths`, with no `exec`, `destroy`, `power`, or `admin`. A real boundary
  for file work through a pinned door.
- **`operator`**: `allow = ["effect:read", "effect:write", "effect:exec", "effect:destroy"]`,
  `deny = ["effect:power", "effect:admin"]`. Everything except power and administration. Because
  `operator` can run commands, it is as strong as the OS account and could change Legion's own
  files (including `policy.toml` and `tools.d/`) through a shell: treat it as a guard rail and an
  audit trail, not a wall.

`legion meta.doctor` warns about every role in `policy.toml` that may run commands, so roles with
`exec` never look like confined sandboxes by accident.

## How each door pins a principal

| Door | How principal is set | Trust boundary |
| --- | --- | --- |
| Local CLI (`legion ...`) | `default` from `policy.toml` (`owner`), or `--as <principal>` | Guard rail and audit trail; the local shell already owns the OS account. |
| Stdio MCP (`legion mcp --as <principal>`) | Pinned for the session by `--as` | Guard rail when launched locally; wall only if the agent runs in a separate OS user or container. |
| Restricted SSH key (`legion gate --as <principal>`) | Forced by `sshd` in `authorized_keys`; pinned for every RPC and stream call | **Wall** (for roles without `exec`). |
| HTTP (`legion serve`) | Looked up from `Authorization: Bearer <token>` in `tokens.toml`; pinned per request | **Wall** (for roles without `exec`). |

### Restricted SSH keys with `legion gate`

To confine an SSH key to one principal on a machine, add a forced command in `~/.ssh/authorized_keys`
(on Windows for an Administrator account, `C:\ProgramData\ssh\administrators_authorized_keys`):

```text
command="/home/me/.local/bin/legion gate --as script:backup",restrict ssh-ed25519 AAAA... backup-key
```

Use the absolute path to `legion`. `restrict` disables port forwarding, agent forwarding, X11
forwarding, and PTY allocation. Whatever command the SSH client asks for, `sshd` starts
`legion gate --as <principal>` and passes the client's request in `SSH_ORIGINAL_COMMAND`. `gate`
never invokes a shell and never executes `SSH_ORIGINAL_COMMAND`: it compares it byte-for-byte
against the exact command shapes Legion's SSH connector sends for `legion rpc` and `legion stream`,
serves the session directly pinned to `<principal>`, and refuses anything else with exit code 1.

### Delegation

Only a role with `delegate = true` (by default, only `owner`) may act as another principal via
`--as` or a request's `as` field. On a door that pinned a principal without `delegate = true`, any
different `as` in a request is ignored; on an unpinned door, it is refused with `denied`.

## Forwarding across nodes

Every node a call passes through applies its own `policy.toml` and writes the call to its own audit
log:

- When `tower` forwards a call to `pi` (or through `pi` to `shed`), `tower` checks its own policy
  first, logs the call with `target = "pi"` (or `"shed"`), and passes the caller's principal to the
  next node.
- The target node resolves that principal in **its own** `policy.toml` and decides independently.
  If the target does not name that principal, it gets `nobody` and the call fails with `denied`.
- If a forwarded call names an extension tool that exists only on the target and whose effect the
  forwarding node cannot determine, the forwarding node fails closed unless its policy denies no
  effects and allows `*`.

## The audit log

Every call — succeeded, failed, or denied — is appended as one JSON object per line to
`state/audit.jsonl` under a kernel file lock (`state/audit.lock`). Inspect it with `audit.tail`:

```sh
legion audit.tail limit=5 --json
```

```json
{
  "entries": [
    {
      "time": "2026-11-24T02:40:52.115Z",
      "principal": "phone",
      "door": "cli",
      "tool": "power.sleep",
      "target": "tower",
      "outcome": "denied",
      "duration": 0.000092,
      "args": {
        "dry_run": true
      },
      "message": "phone may not use power.sleep: its role phone_remote allows no rule that matches it (its effect is power)."
    }
  ],
  "truncated": true,
  "skipped": 0
}
```

Filter by `principal`, `tool` (accepts globs such as `fs.*`), or `outcome` (`ok` or an error code).

### What is redacted

Before writing to `audit.jsonl`, sensitive arguments and error messages are replaced with
`{"redacted_bytes": <size>}`:

- File content fields: `content`, `contents`, `data`, `bytes`, `body`, `stdin`, `old`, `new`.
- Environment maps (`env`): variable names are kept and every value is replaced by its byte length.
- Keys naming a secret (`password`, `passwd`, `passphrase`, `secret`, `token`, `credential`,
  `cookie`, `authorization`, `api_key`, `apikey`, `private_key`, `access_key`, whole keys `auth`,
  `pass`, `pw`, `pwd`, and endings `_key`, `_auth`, `_pass`, matching `-` and `_` case-insensitively).
- Argument values following secret flags (`-p`, `-u`, `--user`, `--password`, `--token`,
  `--api-key`, etc.).
- Strings containing PEM markers (`-----BEGIN`), auth headers (`Bearer `, `Basic `, `Digest `,
  `Token `), known service token prefixes (including Legion's `lgn_`, GitHub `ghp_`/`github_pat_`,
  AWS `AKIA`/`ASIA`, OpenAI/Stripe `sk-`, Slack `xox*`, etc.), JWTs (`eyJ...`), URLs with
  credentials (`https://user:pass@host`), or inline secret assignments (`password=...`, `?token=...`).
- Any string longer than 4 KiB, and the entire `args` object if its redacted JSON exceeds 16 KiB.

## Cookbook

Each recipe shows the `policy.toml` lines plus an allowed and a refused call from the binary.

### 1. Read-only AI agent (`observer`)

Give a reviewing or research agent read access inside your projects folder and nothing else:

```toml
[roles.observer]
allow = ["effect:read"]
deny  = ["audit.*"]
paths = ["~/projects/**"]

[principals."agent:reader"]
role = "observer"
```

Allowed:

```sh
legion fs.read path=~/projects/site/README.md --as agent:reader
```

```text
path      /home/me/projects/site/README.md
size      7
offset    0
length    7
encoding  utf8
content   readme
eof       true
hash      blake3:362a227ca1c142abda74c1784d3c02588a57b2612e3f5367f79d15562492e97b
```

Refused:

```sh
legion fs.write path=~/projects/site/README.md content="nope" --as agent:reader
```

```text
legion: denied: agent:reader may not use fs.write: its role observer allows no rule that matches it (its effect is write).
  Ask the owner of this machine to change policy.toml if it should be allowed.
```

### 2. Agent that may work in one project folder (`editor`)

Give an agent read and write access confined to one project directory, without `exec` or `destroy`:

```toml
[roles.editor]
allow = ["effect:read", "effect:write"]
deny  = ["audit.*", "fs.remove"]
paths = ["~/projects/site/**"]

[principals."agent:editor"]
role = "editor"
```

Allowed:

```sh
legion fs.write path=~/projects/site/notes.txt content="hello\n" --as agent:editor
```

```text
path     /home/me/projects/site/notes.txt
size     7
hash     blake3:d54b10f4bff80548f514361f83e4252cc57efbd3513bee9e4651dd00cda974d6
created  true
```

Refused (outside `paths`):

```sh
legion fs.read path=~/private/secrets.txt --as agent:editor
```

```text
legion: denied: agent:editor may not read /home/me/private/secrets.txt: its role editor confines file tools to ~/projects/site.
  Use a path under one of those directories, or ask the owner to add the directory to the role's paths in policy.toml.
```

### 3. Agent you would hand a terminal (`operator`)

If an agent needs to build, run tests, and start jobs, use `operator`. Be explicit about what that
means: because it allows `exec`, it is as strong as your OS account. It stops accidental `power.*`
or `admin` calls and gives every call its own principal in `audit.jsonl`, not an OS sandbox.

```toml
[roles.operator]
allow = ["effect:read", "effect:write", "effect:exec", "effect:destroy"]
deny  = ["effect:power", "effect:admin"]

[principals."agent:claude"]
role = "operator"
```

Allowed:

```sh
legion exec.run argv='["uname","-s"]' --as agent:claude
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

Refused:

```sh
legion token.list --as agent:claude
```

```text
legion: denied: agent:claude may not use token.list: its role operator denies it with the rule effect:admin.
  Ask the owner of this machine to change policy.toml if it should be allowed.
```

### 4. Phone that may check status and wake a machine

Pair this role with an HTTP bearer token (`legion token add phone phone`) behind Tailscale Serve:

```toml
[roles.phone_remote]
allow = [
  "meta.version",
  "meta.whoami",
  "sys.info",
  "fleet.list",
  "fleet.ping",
  "power.status",
  "power.wake",
  "power.wol",
]

[principals.phone]
role = "phone_remote"
```

Allowed:

```sh
legion power.status --as phone
```

```text
boot_id  00000000-0000-4000-8000-000000000001
busy
  state    idle
  reasons  -
targets  -
sudo     false
```

Refused:

```sh
legion power.sleep dry_run=true --as phone
```

```text
legion: denied: phone may not use power.sleep: its role phone_remote allows no rule that matches it (its effect is power).
  Ask the owner of this machine to change policy.toml if it should be allowed.
```

### 5. Restricted SSH key for a backup script

In `policy.toml`:

```toml
[roles.backup_script]
allow = ["fs.list", "fs.stat", "fs.read", "fs.hash", "fs.manifest"]
paths = ["~/projects/site/**"]

[principals."script:backup"]
role = "backup_script"
```

In `~/.ssh/authorized_keys` on the machine being backed up:

```text
command="/home/me/.local/bin/legion gate --as script:backup",restrict ssh-ed25519 AAAA... backup@nas
```

Refused (arbitrary shell command over SSH):

```sh
SSH_ORIGINAL_COMMAND="rm -rf /" legion gate --as script:backup
```

```text
legion gate: this key may only run `legion rpc` or `legion stream`, exactly as legion's ssh connector sends them; nothing else is run.
```

Refused (RPC call outside the role's allow list, pinned to `script:backup`):

```json
{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"script:backup may not use meta.whoami: its role backup_script allows no rule that matches it (its effect is read).","data":{"code":"denied","message":"script:backup may not use meta.whoami: its role backup_script allows no rule that matches it (its effect is read).","hint":"Ask the owner of this machine to change policy.toml if it should be allowed.","machine":"tower","details":{"principal":"script:backup","role":"backup_script","tool":"meta.whoami"}}}}
```

Allowed: `legion cp tower:~/projects/site /home/me/backups/site` using that SSH key (each `stream`
read operation is checked against `fs.read` and confined to `~/projects/site`).

### 6. What NOT to do: giving `exec` to a role you want confined

Do not combine `effect:exec` with `paths` expecting `paths` to lock the role inside a folder:

```toml
# Do NOT treat this as a sandbox.
[roles.leaky]
allow = ["effect:read", "effect:write", "effect:exec"]
paths = ["~/projects/site/**"]

[principals."agent:leaky"]
role = "leaky"
```

`fs.read` is blocked outside `~/projects/site`:

```sh
legion fs.read path=~/private/secrets.txt --as agent:leaky
```

```text
legion: denied: agent:leaky may not read /home/me/private/secrets.txt: its role leaky confines file tools to ~/projects/site.
  Use a path under one of those directories, or ask the owner to add the directory to the role's paths in policy.toml.
```

Yet `exec.run` reads the same file directly because the OS process runs as the account:

```sh
legion exec.run argv='["cat","/home/me/private/secrets.txt"]' --as agent:leaky
```

```text
top secret
exit_code         0
stdout            top secret
stderr            
stdout_truncated  false
stderr_truncated  false
timed_out         false
duration_ms       0
```

`legion meta.doctor` flags roles that may run commands so this mismatch is never silent. If you
need a real boundary, remove `exec` (and `job.*` / `proj.run` / `svc.*`) and use `editor` or
`observer`.
