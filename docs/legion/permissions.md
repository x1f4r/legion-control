# Permissions

Each Legion node enforces its own access rules from `policy.toml` in its configuration directory
and records every call in `state/audit.jsonl`.

## The central rule

**A role that may run commands (`exec`) is as strong as the operating system account.** A command
can read, write, and start anything the OS lets that user touch — including Legion's own
`policy.toml`, `tokens.toml`, and `audit.jsonl`. `paths` does not contain `exec`; it only chooses
where a command starts.

**A role with `effect:admin` (or `config.write`) is as strong as `owner`.** It can rewrite
`policy.toml` and change who may do what on the machine — including giving itself `exec` or full
access.

**Real boundaries require both:**

1. A role **without** the `exec` or `admin` effects (such as `observer` or `editor`), and
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
| `read` | `meta.whoami`, `fs.read`, `sys.info`, `svc.status`, `power.status`, `audit.tail`, `device.endpoint` | Inspects state without changing files, processes, services, or power state. |
| `write` | `fs.write`, `fs.edit`, `fs.copy`, `fs.move`, `fs.mkdir`, `fs.transfer`, `proj.add` | Creates or modifies files and project entries. **`write` can replace content**: denying `fs.remove` prevents deleting files, not overwriting their bytes with something else. |
| `destroy` | `fs.remove`, `sys.kill` | Removes files or directories or terminates processes. Replacing a whole directory counts as destroying it. |
| `exec` | `exec.run`, `job.start`, `job.cancel`, `svc.start`, `svc.stop`, `svc.restart`, `proj.run`, `proj.clone`, `proj.pull` | Runs programs on the machine. **`exec` is as strong as the account**: `paths` does not contain what a command reads, writes, or launches. |
| `power` | `power.sleep`, `power.restart`, `power.shutdown`, `power.cancel`, `power.wol`, `power.wake`, `power.switch` | Changes machine power state, calls a pending change off, or sends Wake-on-LAN packets. |
| `admin` | `meta.init`, `meta.update`, `fleet.add`, `fleet.remove`, `fleet.install`, `fleet.sync`, `config.*`, `token.*`, `device.*`, `ext.define`, `ext.remove`, `agent.*` | Changes Legion itself: policy, tokens, paired devices, fleet membership, extensions, synced config, agent registrations, and the binary. **`admin` is as strong as `owner`**: it can rewrite `policy.toml` (via `config.write`) and change who may do what on this machine. |

### Legion's own files belong to `admin`

No file tool (`fs.*` or `legion stream`) may read or write the node's configuration directory or
state directory for a role without `effect:admin`, regardless of what `paths` says. A role that can
write files cannot edit `policy.toml` or `devices.toml`, inspect `tokens.toml`, or tamper with
`audit.jsonl` through Legion's file tools.

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

Even when a role has broad or unconfined `paths`, Legion refuses file-tool access (`fs.*` and
`legion stream`) — and skips entries during recursive walks (`fs.find`, `fs.grep`, `fs.manifest`,
and directory transfers) — for a built-in list of well-known secret locations under `~` for every
role that does not allow `effect:admin`:

- **SSH and GPG**: `~/.ssh`, `~/.gnupg` (plus `%ProgramData%\ssh` on Windows, any path configured
  in `[devices] authorized_keys` in `node.toml`, any symlinked `~/.ssh/authorized_keys` target
  path, and the `.legion.lock` / `.legion-tmp-*` atomic-write siblings beside those files).
- **Cloud, container, and git credentials**: `~/.aws`, `~/.azure`, `~/.kube`,
  `~/.docker/config.json`, `~/.netrc`, `~/.git-credentials`, `~/.config/gh`, `~/.config/gcloud`.
- **Password stores and keyrings**: `~/.password-store`, `~/.local/share/keyrings`,
  `~/Library/Keychains`.
- **AI coding agent credentials**: `~/.claude.json`, `~/.claude/.credentials.json`,
  `~/.codex/auth.json`, `~/.gemini/oauth_creds.json`, `~/.gemini/google_accounts.json`,
  `~/.config/opencode/service.json`, `~/.local/share/opencode/auth.json`,
  `~/.config/github-copilot`.
- **Browser profiles**: Firefox, Chrome, Chromium, Brave, and Edge profile directories across
  Linux, macOS, and Windows.

A broad root such as `paths = ["~/**"]` does **not** opt in: to allow a non-admin role to touch one
of these locations, the role's `paths` must explicitly name the location (or a path inside it). This
protection applies to file tools only and does not bind a role with `exec`.

```sh
legion fs.read path=~/.ssh/id_ed25519 --as agent:wide
```

```text
legion: denied: agent:wide may not read ~/.ssh/id_ed25519: ~/.ssh is on Legion's list of secret locations.
  Only roles with effect:admin may touch secret locations by default. To allow this role, the owner can name ~/.ssh (or a path inside it) explicitly in the role's `paths` in policy.toml.
```

The list also covers package-registry credentials (`~/.cargo/credentials.toml`, `~/.pypirc`),
`~/.config/rclone`, `~/.config/op`, `~/.vault-token`, Terraform's credentials file, mail profiles
and, on Windows, the operating system's credential stores. `.npmrc` and shell history files are
deliberately left out because they cause too many false positives. An owner can protect them, or
any other path, with a top-level `secrets = ["~/extra/path"]` in `policy.toml`; such a path is
treated exactly like the built-in list.

Two limits remain. A hard link to a secret file made outside Legion cannot be detected by any path
check. And a role without the admin effect cannot create a secret location that does not exist
yet, or a link in its place: planting `~/.ssh/authorized_keys` would be as bad as reading a key.

## The three ready-made roles

`legion init` writes a starter `policy.toml` with `default = "owner"` and three roles:

- **`observer`**: `allow = ["effect:read"]`, `deny = ["audit.*"]`, `paths = ["~/projects/**"]`.
  May look without changing anything: system facts, files under `paths`, job, service and project
  status, and also every process's command line (with secret-looking values replaced), service
  logs, every principal's job commands and output, the fleet's layout and the definitions of
  extension tools. It can run any extension tool declared `read`. Never Legion's audit log. A real boundary when paired with a pinned door.
- **`editor`**: `allow = ["effect:read", "effect:write"]`,
  `deny = ["audit.*", "proj.add", "proj.remove", "fs.transfer"]`, `paths = ["~/projects/**"]`. May
  read and modify files inside `paths`, with no `exec`, `destroy`, `power`, or `admin`; it cannot
  change which projects are registered or send files to other machines. A real boundary for file
  work through a pinned door.
- **`operator`**: `allow = ["effect:read", "effect:write", "effect:exec", "effect:destroy"]`,
  `deny = ["effect:power", "effect:admin"]`. Everything except power and administration. Because
  `operator` can run commands, it is as strong as the OS account and could change Legion's own
  files (including `policy.toml` and `tools.d/`) through a shell: treat it as a guard rail and an
  audit trail, not a wall.

`legion meta.doctor` warns about every non-owner role in `policy.toml` that may run commands —
whether or not it sets `paths`, and even if it denies `effect:admin` — so roles with `exec` never
look like confined sandboxes by accident.

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
ignores `LEGION_HOME`, `XDG_CONFIG_HOME`, and `XDG_STATE_HOME` from the SSH environment (relying
only on explicit `--home` or `--config-dir`/`--state-dir` flags or the account's standard paths, and
refusing to serve if a pinned home is missing rather than recreating it), never invokes a shell, and
never executes `SSH_ORIGINAL_COMMAND`: it compares it byte-for-byte against the exact command shapes
Legion's SSH connector sends for `legion rpc` and `legion stream`, serves the session directly
pinned to `<principal>` with `door: "ssh"`, and refuses anything else with exit code 1.

### Paired devices

Paired devices are kept in `devices.toml` beside `policy.toml`, and every node derives its own
`authorized_keys` device lines from `devices.toml`. On first run (when `devices.toml` does not
exist yet), Legion adopts only pre-existing `--pairing` lines that match a record in this node's
own `state/devices.json` (with equal principal, key fingerprint, and pairing id); any other
Legion-shaped line is preserved byte for byte as local-only (kept working locally, never offered to
peers, flagged as `local_only: true` in `device.list`, refused by `device.set`, and removable via
`device.remove`). A device paired with `legion pair` (`device.invite`) chooses a role from
`policy.toml` (creating its dedicated `[principals."device:<name>"]` entry atomically at enrolment,
not at invite time) or pairs under an existing principal via `--as <principal>`. Its derived
`authorized_keys` line forces `legion gate --as <principal> --device <name> --pairing <id>` with
`restrict`, always pinning a random per-pairing id and the node's resolved `--home` or
`--config-dir`/`--state-dir` (plus `--account` on Windows's shared
`administrators_authorized_keys`). Legion only classifies a line as managed when its option list
and forced command match byte-for-byte what Legion emits for the host shell
(`restrict,command="..."` for devices, `restrict,expiry-time="<12-digits>",command="..."` for
invites, with matching `--device`/`--enrol` and `legion-device:`/`legion-invite:` comment, and no
extra or reordered options), leaving every other line untouched byte for byte. Once paired, the
device can only speak `legion rpc` and `legion stream` pinned to `<principal>` (reported and
audited with `door: "ssh"` and `device: "<name>"`) — never an interactive shell, arbitrary command,
PTY, or forwarding — and may do only what `<principal>`'s role in `policy.toml` allows (a real wall
for roles without `exec`, and still as strong as the OS account if given `exec`). Requests naming a
different `as` are ignored, and calls forwarded to other nodes with `on` are re-checked by each
target node's own `policy.toml`.

During pairing itself, the temporary key in the invite is single-use, carries a portable 12-digit
`expiry-time` upper bound for OpenSSH 8.9+ compatibility, and is forced to
`legion gate --enrol <id>` (ignoring `SSH_ORIGINAL_COMMAND`). Exact expiry is enforced by
`legion gate --enrol` against `state/devices.json`, so an expired temporary key stops working at
its exact expiry time and its line is swept on the next `device.*` command. Enrolment also refuses
the invite's own temporary key and any key already present in `authorized_keys` or `devices.toml`.
Note that the invite text is a secret until used and is returned by `device.invite`; if an AI agent
invokes `device.invite`, the live invite enters the agent's transcript, so run `legion pair` in
your own terminal when you want to keep it out of agent transcripts.

Creating, listing, updating, or removing invites and paired devices (`device.invite`,
`device.list`, `device.set`, `device.remove`) requires `effect:admin`. `device.endpoint` is
`effect:read` so paired controllers can discover fleet endpoints: any role with `read` learns the
node name, candidate host names / Tailscale addresses, SSH port, OS account name, and public sshd
host keys (with auto-discovered host-key scans cached briefly per node, and without exposing how
host keys were resolved). Device management follows the same caller-scoping and `allow_owner` rules
as `token.*`: creating a `device:<name>` principal or changing a device's role with `device.set`
requires the `owner` role and `allow_owner=true` to grant `owner`. A paired device may create
pairing invites only on the node it is connected to directly: forwarding `device.invite` (`on=`)
from a paired device is refused with a hint to connect to that machine directly so `paired_by.device`
is always recorded from the session context. Expiring credentials (such as expiring HTTP bearer
tokens) cannot mint or modify permanent device access: locally, `device.invite`, `device.set`, and
any `devices.toml` merge that adds or changes a live entry are refused (pure tombstones are still
allowed locally so an expiring credential can revoke a device); across remote hops (`on`),
forwarding `device.invite`, `token.add`, `device.set`, `config.write` for `devices.toml` or
`policy.toml`, and `fleet.sync` `push`/`pull` is refused by the forwarding node because credential
expiry is not carried over RPC.

**Trust rule for syncing paired devices:** a node trusts a paired device only if (a) it paired the
device itself, (b) a caller with accepted credentials pushed it via `config.write` under this
node's `policy.toml`, or (c) an owner on this node explicitly pulled it with
`fleet.sync direction=pull from=<machine> documents='["devices.toml"]'`. Pushing
(`fleet.sync direction=push`, and the automatic pushes after enrolment, `device.set`, and
`device.remove`) sends local entries to peers and imports **only tombstones** back — never live
device entries from a peer — so syncing out to a less-trusted or compromised peer cannot silently
inject keys back onto the pushing machine. `fleet.sync direction=status` compares read-only.

The registry owns only `device:*` principals it created for one pairing id — marked with
`device = "<pairing id>"` in `policy.toml` — and refuses creating tokens, agents, or `--as`
pairings on a registry-owned principal. Each registry-owned `device:*` principal belongs to
**exactly one** pairing id across the lifetime of the fleet: every merge (including owner writes)
refuses assigning a marked, live, or previously tombstoned `device:*` principal to a different
pairing id, and reconciliation never re-creates a `device:*` principal for a different id than the
one that owned it. Merging `devices.toml` via `fleet.sync` or `config.write` refuses incoming
timestamps more than 10 minutes in the future, refuses introducing duplicate live public keys,
always merges with the local file (tombstones always win and can never be dropped by syncing),
refuses setting `confirmed_by`/`confirmed_at` or dropping `paired_by` on an existing entry even for
owner callers (confirmation is only allowed through `device.set confirm=true`), restricts non-owner
admins to devices pinned to their own principal (recording `paired_by` from the call's context —
including the caller's pairing id when the caller is a paired device — and refusing entries that
claim otherwise, while treating `paired_at`, `paired_on`, `paired_by`, and `public_key` as
immutable once written), pushes non-owner revocations independently of unrelated entries, and
reconciles only registry-owned `device:*` principals in `policy.toml` to the exact recorded role —
leaving shared and unmarked `device:*` principals untouched and failing closed to `nobody` when a
role is undefined on the receiving node or two live devices collide on one `device:*` principal. A
device whose `device:*` principal is unmarked, marked for a different pairing id, or assigned a
different role in `policy.toml` than recorded in `devices.toml` (so a recorded role can never be
exceeded by what `policy.toml` happens to say) receives no `authorized_keys` line and is denied by
`legion gate` (the same treatment as a key collision), with warnings in results, `device.list`, and
`meta.doctor`. If two live entries in `devices.toml` ever share a public key (or collide with a
foreign line in `authorized_keys`), no `authorized_keys` line is written for any colliding entry
and both `device.list` and `meta.doctor` warn loudly. Tombstones in `devices.toml` are permanent so
removed devices cannot be resurrected and quarantine cannot be lifted by pruning.

Each device records `paired_by = { principal, device }` captured from the invite creator (or filled
from the call's context when added via `config.write`). When a device is tombstoned, any live
descendant with `paired_by.device == <tombstoned id>` and
`paired_at >= tomb.removed_at - 10 minutes` (matching the 10-minute clock-skew bound) — and
transitively its unconfirmed descendants — is automatically quarantined (omitted from
`authorized_keys`, restricted to `nobody`, refused by `legion gate`, and flagged loudly in
`device.list` and `meta.doctor`) until an owner explicitly confirms it locally on that node with
`legion device.set id=<pairing_id> confirm=true` (which is refused unless the device is quarantined
right now and records `confirmed_by` and `confirmed_at > tomb.removed_at`).

Removing a device with `legion device.remove` writes the tombstone to `devices.toml` first before
cleaning up its unused `device:*` principal and `authorized_keys` line, immediately terminates any
live gate processes (after verifying each target PID runs this `legion` executable) and verified
same-account parent `sshd` connections (disambiguated by pairing id, including multiplexed
`ControlMaster` connections, reporting `sessions_ended`, even if a later reconciliation step fails
after `devices.toml` was saved), reports any still-running background jobs started by that device
in `jobs_left_running` without killing them, and pushes the tombstone to reachable peers in
`fleet.toml` (any peer that receives a tombstone via `fleet.sync` / `config.write` immediately
ends that device's active sessions too). **Honest limit without a central hub:** a removed device
that held an `owner` or `exec` role can, on any machine that has not yet heard about the removal,
still act with that machine's own authority until the tombstone arrives (including creating further
devices, which are quarantined once the removal syncs unless the machine itself was altered outside
Legion). Removal is complete only when every machine in the fleet has received the tombstone, and
`device.remove` names every machine that was not reached in `still_trusted_by` (never silently
returning an empty map when a push fails or cannot run). After registering its session record,
`legion gate` verifies the on-disk `authorized_keys` public key against `devices.toml`, checks
quarantine and key conflicts, and terminates its verified same-account parent `sshd` only on
`Denied`, so neither a newly opened channel on an already-open connection, a stale key on disk,
nor a later re-pair under the same name can revive a removed pairing, while transient non-`Denied`
errors refuse only that session without tearing down an unrelated healthy connection. Owner callers
can inspect foreign or orphaned Legion lines in `device.list` and remove them explicitly with
`device.remove ... foreign=true` (`meta.doctor` warns when orphaned keys exist, when devices are
quarantined, when keys collide, or when peers have not synced yet; removing a foreign device
deletes its key without ending sessions tracked under that other home and reports
`sessions_not_ended: true`). On Windows, forced commands on shared
`administrators_authorized_keys` pin `--account`, set file ownership to `BUILTIN\Administrators`,
refuse PowerShell Unicode single-quote delimiters (`U+2018`–`U+201B`), and scope `device.list` /
`device.remove` to the caller's own account.

### HTTP bearer tokens and MCP sessions

Tokens in `tokens.toml` (managed with `token.add`, `token.list`, and `token.remove`, all requiring
`effect:admin`) obey four rules:

1. **Caller scoping**: a caller may create, list, or remove tokens only for a principal it could
   itself act as (its own principal, or any principal when the caller has the `owner` role).
2. **Owner-role tokens**: minting a token for any principal whose role is `owner` requires a caller
   with the `owner` role **and** `allow_owner=true`:

   ```sh
   legion token add my-owner owner
   ```

   ```text
   legion: denied: owner has the owner role: a token for it may do everything the account can, from anywhere the server is published, so it is not made unless asked for in so many words.
     Give the token a principal with a narrower role in policy.toml, or pass allow_owner=true if this is really what you want.
   ```

3. **Expiry ceiling**: a token minted through an expiring HTTP token cannot outlive the caller's
   token (omitting `expires_days` or asking for a later expiry fails with `denied`).
4. **MCP session ownership**: streamable-HTTP MCP sessions on `/mcp` belong to the token that opened
   them, and another token presenting the same `Mcp-Session-Id` is turned away as if the session did
   not exist. Limits are 16 sessions per token (opening a 17th ends that token's least recently used
   session), 256 sessions total across the server, and a 5-minute idle timeout. Removing, replacing,
   or expiring a token closes all of its MCP sessions immediately.

### Delegation

Only the built-in `owner` role may act as another principal via `--as` or a request's `as` field. A
role that could act as anyone would be `owner` under another name, so a `policy.toml` that sets
`delegate = true` on any other role is refused when it is loaded (give the principal `role = "owner"`
if full access is intended):

```text
legion: failed: /home/me/.config/legion/policy.toml: [roles.helper] sets delegate = true, but acting as another principal is the owner's alone: a role that could act as anyone would be the owner under another name; give the principal role = "owner" if that is what is meant.
  Fix the file. Until it parses, every call that needs it fails rather than using a default.
```

On a door that pinned a non-owner principal, any different `as` in a request is ignored; on an
unpinned door, it is refused with `denied`.

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
legion: denied: agent:editor may not read ~/private/secrets.txt: its role editor confines file tools to ~/projects/site.
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
legion: denied: agent:leaky may not read ~/private/secrets.txt: its role leaky confines file tools to ~/projects/site.
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
