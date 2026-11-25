// Exact 78-tool catalogue reported by Legion build 05ba1ef2e385 (matches apps/ui/test/fixtures/tools-05ba1ef2e385.json)
// plus strict argument validation producing the same invalid_args errors as the real node.

export const OLD_FIXTURE_VERSION = '0.1.0';
export const OLD_FIXTURE_BUILD = '05ba1ef2e385';

export const OLD_FIXTURE_TOOLS = [
  {
    "name": "agent.add",
    "description": "Register this `legion` binary as an MCP server named `legion` with an AI coding agent using the agent's own CLI command, never by editing its configuration files directly. `compact` defaults to true because every registered tool schema costs the agent context on every conversation; pass `compact=false` to expose every tool individually. A named, limited principal from policy.toml (such as `agent:claude` with the `editor` or `observer` role) is safer than `owner` because it narrows what the agent may do and identifies its calls in the audit log; registering as `owner` requires `allow_owner=true`.",
    "effect": "admin",
    "input_schema": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "agent": {
          "description": "Which AI coding agent to register Legion with (`claude`, `codex`, `antigravity`, `opencode`).\n`claude`: Claude Code (`claude`).\n`codex`: OpenAI Codex CLI (`codex`).\n`antigravity`: Antigravity CLI (`agy`).\n`opencode`: OpenCode (`opencode`).",
          "type": "string",
          "enum": [
            "claude",
            "codex",
            "antigravity",
            "opencode"
          ]
        },
        "as": {
          "type": "string",
          "description": "Principal every MCP call from this agent acts as. Must be named in policy.toml, or `owner`\nwith `allow_owner=true`. Prefer a named, limited principal so the agent is confined by its\nrole and clearly identified in the audit log."
        },
        "compact": {
          "type": "boolean",
          "description": "Pass `--compact` to `legion mcp` (exposes four meta-tools instead of one per registered\ntool). Defaults to true because every registered tool schema costs context on every\nconversation.",
          "default": true
        },
        "scope": {
          "type": "string",
          "description": "Scope for agents that support one (`local`, `user`, or `project` for `claude`, default\n`user`; `global`/`user` or `project`/`local` for `opencode`, default `global`)."
        },
        "allow_owner": {
          "type": "boolean",
          "description": "Allow registering the agent as `owner`, which grants unrestricted access. Off unless true.",
          "default": false
        },
        "dry_run": {
          "type": "boolean",
          "description": "Return the exact registration command without running it.",
          "default": false
        }
      },
      "required": [
        "agent",
        "as"
      ]
    }
  },
  {
    "name": "agent.list",
    "description": "Check each known AI coding agent (claude, codex, antigravity, opencode) to see whether its CLI is on PATH and whether an MCP server named `legion` is registered via its own `mcp list` command.",
    "effect": "admin",
    "input_schema": {
      "type": "object",
      "additionalProperties": false
    }
  },
  {
    "name": "agent.remove",
    "description": "Remove the `legion` MCP server registration from an AI coding agent using the agent's own CLI command, never by editing its configuration files directly.",
    "effect": "admin",
    "input_schema": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "agent": {
          "description": "Which AI coding agent to remove Legion from (`claude`, `codex`, `antigravity`, `opencode`).\n`claude`: Claude Code (`claude`).\n`codex`: OpenAI Codex CLI (`codex`).\n`antigravity`: Antigravity CLI (`agy`).\n`opencode`: OpenCode (`opencode`).",
          "type": "string",
          "enum": [
            "claude",
            "codex",
            "antigravity",
            "opencode"
          ]
        },
        "scope": {
          "type": "string",
          "description": "Scope for `claude` (`local`, `user`, or `project`; defaults to `user`)."
        },
        "dry_run": {
          "type": "boolean",
          "description": "Return the exact removal command without running it.",
          "default": false
        }
      },
      "required": [
        "agent"
      ]
    }
  },
  {
    "name": "audit.tail",
    "description": "Show the last calls this node recorded in its audit log, oldest first: when, by whom, through which door, which tool, how it ended and how long it took, with file contents and secrets in the arguments replaced by their size. Filters narrow it to a principal, a tool glob or an outcome.",
    "effect": "read",
    "input_schema": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "limit": {
          "type": "integer",
          "default": 20,
          "minimum": 1,
          "description": "How many entries to return, from 1 to 1000.",
          "maximum": 1000
        },
        "principal": {
          "type": "string",
          "description": "Only calls that acted as this principal, such as `agent:claude`."
        },
        "tool": {
          "type": "string",
          "description": "Only calls to tools this glob matches, such as `fs.*`."
        },
        "outcome": {
          "type": "string",
          "description": "Only calls that ended this way: `ok`, or an error code such as `denied`."
        }
      }
    }
  },
  {
    "name": "config.list",
    "description": "List the shared configuration documents present on this node (devices.toml, fleet.toml, policy.toml, projects.toml, and files in tools.d/). Machine-local files (node.toml, tokens.toml) are never included.",
    "effect": "admin",
    "input_schema": {
      "type": "object",
      "additionalProperties": false
    }
  },
  {
    "name": "config.read",
    "description": "Read one shared configuration document (devices.toml, fleet.toml, policy.toml, projects.toml, or tools.d/<name>.toml) and its BLAKE3 content hash. Refuses machine-local files such as node.toml and tokens.toml.",
    "effect": "admin",
    "input_schema": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "document": {
          "type": "string",
          "description": "Shared document name: `devices.toml`, `fleet.toml`, `policy.toml`, `projects.toml`, or `tools.d/<name>.toml`."
        }
      },
      "required": [
        "document"
      ]
    }
  },
  {
    "name": "config.write",
    "description": "Validate and atomically update one shared configuration document (devices.toml, fleet.toml, policy.toml, projects.toml, or tools.d/<name>.toml), keeping the previous version as a backup in the state directory and recording the hash before and after in the audit log. Writing devices.toml directly requires the owner role on this node: it merges incoming entries with local paired devices (tombstones always win and rewrite identity fields from matching local entries), reconciles registry-owned device:* principals (marked with `device = \"<pairing id>\"` in policy.toml) without touching shared or unmarked principals, updates authorized_keys, and ends active sessions for newly removed devices. Writing policy.toml changes who may do what on this machine, so any role allowed to call config.write (including effect:admin) is as strong as owner. Refuses invalid documents before replacing anything, refuses mismatched expect_hash or expect_absent (when a file already exists) with conflict, and refuses machine-local files such as node.toml and tokens.toml.",
    "effect": "admin",
    "input_schema": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "document": {
          "type": "string",
          "description": "Shared document name: `devices.toml`, `fleet.toml`, `policy.toml`, `projects.toml`, or `tools.d/<name>.toml`."
        },
        "content": {
          "type": "string",
          "description": "Full UTF-8 TOML text to validate and write atomically."
        },
        "expect_hash": {
          "type": "string",
          "description": "Expected BLAKE3 hash (`blake3:<hex>`) of the existing file; fails with `conflict` if different."
        },
        "expect_absent": {
          "type": "boolean",
          "description": "Require that the document does not currently exist; fails with `conflict` if already present.",
          "default": false
        },
        "device_registry_push": {
          "type": "object",
          "additionalProperties": false,
          "properties": {
            "trigger": {
              "type": "string",
              "description": "Local device tool that triggered this push (`device.enrol`, `device.set`, or `device.remove`)."
            },
            "principal": {
              "type": "string",
              "description": "Principal that triggered the local device tool (or created the invite, for `device.enrol`)."
            },
            "device": {
              "type": "string",
              "description": "Paired device name that triggered the local device tool, when known."
            },
            "pairing_id": {
              "type": "string",
              "description": "Pairing identifier of the paired device that triggered the local device tool, when known."
            },
            "source_node": {
              "type": "string",
              "description": "Node name where the local device tool ran."
            }
          },
          "required": [
            "trigger"
          ],
          "description": "Optional audit metadata attached during a node-to-node device-registry push after a local\ndevice tool (`device.enrol`, `device.set`, or `device.remove`). Valid only for `devices.toml`\nand grants no additional permission."
        }
      },
      "required": [
        "document",
        "content"
      ]
    }
  },
  {
    "name": "device.endpoint",
    "description": "Return how a paired device reaches this node over SSH: node name, candidate host names or Tailscale addresses, SSH port, OS account name, and pinned sshd public host keys. Uses the same detection as device.invite (honouring any [devices] settings in node.toml, caching auto-discovered host-key scans briefly per node). Call with on=\"all\" through any node to learn every machine's direct SSH endpoint.",
    "effect": "read",
    "input_schema": {
      "type": "object",
      "additionalProperties": false
    }
  },
  {
    "name": "device.invite",
    "description": "Create a single-use pairing invite for a remote device to connect over SSH. Either give `role` and `name` (owner role only: reserves a unique `device:<name>` principal and creates it in policy.toml when the device enrols, leaving nothing in policy.toml if unused or expired) or `principal` (an existing non-device-owned principal that policy.toml names and the caller could act as itself). Appends a temporary key with sshd's expiry-time and a forced enrolment command to authorized_keys, and returns the invite text once (never logged or stored). Inviting for the owner role or any admin (owner-strength) role requires an owner caller and allow_owner=true. A paired device may create invites only on the node it is connected to directly (not forwarded via `on=`).",
    "effect": "admin",
    "input_schema": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "principal": {
          "type": "string",
          "description": "Existing principal in policy.toml the paired device will act as. Give either `principal`,\nor both `role` and `name`."
        },
        "role": {
          "type": "string",
          "description": "Role from policy.toml (or built-in `owner`, or `nobody`) for a new `device:<name>`\nprincipal created atomically in policy.toml. Requires `name` and an owner caller (plus\n`allow_owner=true` for `owner` or any admin owner-strength role)."
        },
        "name": {
          "type": "string",
          "description": "Device name used to create principal `device:<name>` when pairing with `role`."
        },
        "valid_minutes": {
          "type": "integer",
          "default": 15,
          "minimum": 1,
          "description": "Minutes until the invite expires, from 1 to 1440 (default 15).",
          "maximum": 1440
        },
        "host": {
          "type": "array",
          "items": {
            "type": "string"
          },
          "description": "Host name or IP address (a single string or a list of strings) to put in the invite,\noverriding `[devices] hosts` in `node.toml` and automatic Tailscale/hostname detection."
        },
        "port": {
          "type": "integer",
          "description": "SSH port on this machine (defaults to `[devices] port` in `node.toml`, or 22).",
          "minimum": 1,
          "maximum": 65535
        },
        "user": {
          "type": "string",
          "description": "OS account name on this machine (defaults to `[devices] user` in `node.toml` or the account\nlegion runs as)."
        },
        "allow_owner": {
          "type": "boolean",
          "description": "Allow inviting a device with the `owner` role or any admin (owner-strength) role, which can\nchange who may do what. Off unless given as true.",
          "default": false
        }
      }
    }
  },
  {
    "name": "device.list",
    "description": "List paired devices (from devices.toml, adopting state-matched --pairing lines on first run and listing any unmatched local-only lines) and pending pairing invites for principals the caller could act as, reconciling authorized_keys with devices.toml and sweeping any expired or used invites. For owner callers, also reports Legion lines in authorized_keys belonging to another or missing Legion home (marked with `foreign`, `missing_home`, and `home`).",
    "effect": "admin",
    "input_schema": {
      "type": "object",
      "additionalProperties": false
    }
  },
  {
    "name": "device.remove",
    "description": "Remove a paired device by name or pairing id, or revoke a pending pairing invite by id, for a principal the caller could act as. Records a permanent tombstone in devices.toml, reconciles authorized_keys, and immediately ends live sessions and same-account SSH connections on this node. A removed device is still accepted by any other machine in the fleet that has not yet synced the removal; unreachable machines are named in `still_trusted_by`. Detached background jobs the device started keep running and are listed in `jobs_left_running` (stop them separately with `job.cancel` if needed). Pass `foreign=true` (owner role only) to remove a Legion line belonging to another or missing Legion home; for foreign devices this removes the key without ending sessions tracked under that other home and reports `sessions_not_ended: true`.",
    "effect": "admin",
    "input_schema": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "name": {
          "type": "string",
          "description": "Paired device name to remove, as `device.list` shows it. Give `name` or `id`, not both."
        },
        "id": {
          "type": "string",
          "description": "Paired device pairing id or pending invite id to remove, as `device.list` shows it. Give\n`name` or `id`, not both."
        },
        "foreign": {
          "type": "boolean",
          "description": "Remove a Legion device or invite line belonging to another or missing Legion home (owner\nrole only). Off unless given as true.",
          "default": false
        }
      }
    }
  },
  {
    "name": "device.roles",
    "description": "Return the roles this caller may assign to a newly paired device on this node: each role's name, one-line plain description (generated with the same analysis as `legion pair`), strongest allowed effect, and whether it is owner-strength (`owner` or any role allowing an `admin` tool). Lists only roles the caller is allowed to hand out (empty for callers without the `owner` role) and exposes nothing else from policy.toml.",
    "effect": "read",
    "input_schema": {
      "type": "object",
      "additionalProperties": false
    }
  },
  {
    "name": "device.set",
    "description": "Change a paired device's role (only for a dedicated `device:*` principal marked with `device = \"<pairing_id>\"` in policy.toml; refuses shared or unmarked principals), rename it, or confirm a currently quarantined descendant (`confirm=true`, owner role only; confirmation records `confirmed_at` locally on this node and is refused if the device is not quarantined) in devices.toml, reconciling authorized_keys and pushing role or name changes to reachable fleet peers. Changing a role or confirming requires the owner role (plus allow_owner=true for `owner` or any admin owner-strength role) and every `device.set` call is refused through an expiring credential.",
    "effect": "admin",
    "input_schema": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "name": {
          "type": "string",
          "description": "Paired device name to update, as `device.list` shows it. Give `name` or `id`, not both."
        },
        "id": {
          "type": "string",
          "description": "Pairing id (32 lowercase hex characters) of the device to update. Give `name` or `id`, not\nboth."
        },
        "role": {
          "type": "string",
          "description": "New role from `policy.toml` (or built-in `owner`, or `nobody`). Updates the device's\nprincipal in `policy.toml` and `devices.toml`. Setting `owner` or any admin\n(owner-strength) role requires `allow_owner=true`."
        },
        "rename": {
          "type": "string",
          "description": "New device name (sanitised to lowercase ASCII letters, digits, and `-`)."
        },
        "allow_owner": {
          "type": "boolean",
          "description": "Allow setting the device's role to `owner` or any admin (owner-strength) role, which can\nchange who may do what. Off unless given as true.",
          "default": false
        },
        "confirm": {
          "type": "boolean",
          "description": "Explicitly confirm a quarantined paired device so it becomes active again (owner role\nonly). Off unless given as true.",
          "default": false
        }
      }
    }
  },
  {
    "name": "exec.run",
    "description": "Run a command that finishes and return its exit code and output. Give `argv` to run a program with exactly these arguments, or `cmd` to hand one line to the platform shell (sh on Linux and macOS, PowerShell on Windows). The command is stopped, with everything it started, when `timeout_s` passes or the call is cancelled; a non-zero exit or a timeout is reported in the result, not as an error. Output is streamed as stdout and stderr events while it runs and capped at `max_output` bytes per stream, keeping the start and the end. The command gets legion's environment (except legion's own LEGION_* settings) plus `env`, as a command typed by the account would. Not for anything meant to keep running, such as a server or a watcher: that belongs to job.start. A command can do anything the account can, so a role's `paths` decide only the directory it starts in (the first of them when `cwd` is absent), and a role that must stay confined should not be given exec.",
    "effect": "exec",
    "input_schema": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "cmd": {
          "type": "string",
          "description": "A command line handed whole to the platform shell: sh on Linux and macOS, PowerShell on\nWindows. Give this or `argv`, not both."
        },
        "argv": {
          "type": "array",
          "items": {
            "type": "string"
          },
          "description": "The program and its arguments, run exactly as given with no shell in between. Give this\nor `cmd`, not both."
        },
        "cwd": {
          "type": "string",
          "description": "The absolute directory to run in. When absent: the first of the role's `paths` if it has\nany, otherwise legion's own."
        },
        "env": {
          "type": "object",
          "additionalProperties": {
            "type": "string"
          },
          "description": "Environment variables to set on top of legion's own."
        },
        "stdin": {
          "type": "string",
          "description": "Text written to the command's standard input, which is then closed. Without it the command\nreads end of input at once."
        },
        "timeout_s": {
          "type": "number",
          "description": "Seconds before the command is stopped, up to 3600; 120 when absent."
        },
        "max_output": {
          "type": "integer",
          "description": "Bytes of output kept per stream, up to 1048576; 65536 when absent. Beyond it the start and\nthe end are kept and the middle is replaced by a note of how much was left out.",
          "minimum": 0
        }
      }
    }
  },
  {
    "name": "ext.define",
    "description": "Define or replace an extension tool in tools.d/<name>.toml. A tool declared `read` is callable by every role that may read, whatever its command does, so declare each tool's effect honestly. Give `run` (exact argv) or `cmd` (platform shell line), never both; caller arguments are never spliced into the command and arrive only as `LEGION_ARG_<NAME>` environment variables (upper-cased) and as one JSON object on stdin, alongside `LEGION_TOOL`, `LEGION_PRINCIPAL` and `LEGION_NODE`. Refuses to overwrite an existing extension unless `replace` is true, and refuses built-in names or reserved Legion families.",
    "effect": "admin",
    "input_schema": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "name": {
          "type": "string",
          "description": "Tool name as `family.verb` in lower case (e.g. `gpu.free`). Cannot be a built-in tool or a\nreserved Legion family."
        },
        "description": {
          "type": "string",
          "description": "What the tool does, shown in tool listings and to AI agents."
        },
        "effect": {
          "description": "What the tool can do to the machine; policy rules and MCP annotations use this effect.\n`read`: Looks without changing anything.\n`write`: Changes files or configuration.\n`exec`: Runs a program.\n`destroy`: Removes or overwrites something that cannot be had back.\n`power`: Sleeps, restarts or powers off a machine.\n`admin`: Changes Legion itself.",
          "type": "string",
          "enum": [
            "read",
            "write",
            "exec",
            "destroy",
            "power",
            "admin"
          ]
        },
        "run": {
          "type": "array",
          "items": {
            "type": "string"
          },
          "description": "Exact program and arguments to run with no shell in between. Give this or `cmd`, not both."
        },
        "cmd": {
          "type": "string",
          "description": "Command line handed to the platform shell (sh on Linux and macOS, PowerShell on Windows).\nGive this or `run`, not both."
        },
        "params": {
          "type": "object",
          "additionalProperties": {
            "type": "object",
            "additionalProperties": false,
            "properties": {
              "type": {
                "description": "Parameter type: `string`, `integer`, `number`, or `boolean`.\n`string`: A UTF-8 string.\n`integer`: A 64-bit signed integer.\n`number`: A finite JSON number.\n`boolean`: A boolean (`true` or `false`).",
                "type": "string",
                "enum": [
                  "string",
                  "integer",
                  "number",
                  "boolean"
                ]
              },
              "description": {
                "type": "string",
                "description": "Human-readable description shown in the tool's schema."
              },
              "required": {
                "type": "boolean",
                "description": "Whether callers must supply this parameter. Defaults to false.",
                "default": false
              },
              "default": {
                "description": "Default value used when the caller omits this parameter. Must match `type` (and `enum` if set).",
                "default": null
              },
              "enum": {
                "type": "array",
                "items": {
                  "type": "string"
                },
                "description": "Allowed string values, only valid when `type` is `string`."
              },
              "large": {
                "description": "How to handle string values over 32 KiB for the environment: `refuse` (default) fails with\n`invalid_args`; `stdin` omits `LEGION_ARG_<NAME>`, sets `LEGION_ARG_<NAME>_OMITTED=1`, and\nstill passes the full value on stdin. Only valid when `type` is `string`.\n`refuse`: Fail the call with `invalid_args` before starting the command (default).\n`stdin`: Omit `LEGION_ARG_<NAME>`, set `LEGION_ARG_<NAME>_OMITTED=1`, and pass the full value only\nin the JSON object on stdin.",
                "type": "string",
                "enum": [
                  "refuse",
                  "stdin"
                ]
              }
            },
            "required": [
              "type"
            ],
            "description": "One parameter specification passed to `ext.define`."
          },
          "description": "Parameters the tool accepts, keyed by parameter name. Values arrive as `LEGION_ARG_<NAME>`\nenvironment variables and as JSON on stdin."
        },
        "timeout_s": {
          "type": "number",
          "description": "Seconds before the command is stopped, up to 3600; 60 when absent."
        },
        "cwd": {
          "type": "string",
          "description": "Optional working directory for the command (absolute path or starting with `~`)."
        },
        "replace": {
          "type": "boolean",
          "description": "Overwrite an existing extension with the same name. Off unless given as true.",
          "default": false
        }
      },
      "required": [
        "name",
        "description",
        "effect"
      ]
    }
  },
  {
    "name": "ext.list",
    "description": "List extension tools loaded from tools.d/ with each tool's declared effect and execution mode (`run` directly or `cmd` through the shell), along with any broken files that failed to parse or validate.",
    "effect": "read",
    "input_schema": {
      "type": "object",
      "additionalProperties": false
    }
  },
  {
    "name": "ext.remove",
    "description": "Remove an extension tool's definition file from tools.d/.",
    "effect": "admin",
    "input_schema": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "name": {
          "type": "string",
          "description": "The extension tool's name, as ext.list shows it."
        }
      },
      "required": [
        "name"
      ]
    }
  },
  {
    "name": "ext.show",
    "description": "Show the full stored definition of one extension tool from tools.d/.",
    "effect": "read",
    "input_schema": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "name": {
          "type": "string",
          "description": "The extension tool's name, as ext.list shows it."
        }
      },
      "required": [
        "name"
      ]
    }
  },
  {
    "name": "fleet.add",
    "description": "Add or replace a machine entry in fleet.toml atomically while preserving comments and unrelated entries.",
    "effect": "admin",
    "input_schema": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "name": {
          "type": "string",
          "description": "Machine name (lower-case letters, digits, `-` and `_`, starting with a letter or digit)."
        },
        "ssh": {
          "type": "string",
          "description": "SSH target accepted by the system ssh client. Give one of `ssh`, `command`, `systems` or `via`."
        },
        "command": {
          "type": "array",
          "items": {
            "type": "string"
          },
          "description": "Exact argv to start `legion rpc`. Give one of `ssh`, `command`, `systems` or `via`."
        },
        "systems": {
          "type": "array",
          "items": {
            "description": "One bootable system of a machine, for `fleet.add`.",
            "type": "object",
            "properties": {
              "name": {
                "description": "Name of this bootable system, such as `\"linux\"` or `\"windows\"`.",
                "type": "string"
              },
              "ssh": {
                "description": "SSH target accepted by the system ssh client. Give this or `command`, not both.",
                "type": "string"
              },
              "command": {
                "description": "Exact argv to start `legion rpc`. Give this or `ssh`, not both.",
                "type": "array",
                "items": {
                  "type": "string"
                }
              },
              "program": {
                "description": "Remote `legion` binary name or path when it is not found the default way.",
                "type": "string"
              },
              "os": {
                "description": "Operating system hint (`\"unix\"` or `\"windows\"`).\n`unix`: Linux, macOS, or another POSIX-like host reached through `sh -c`.\n`windows`: Windows host where `legion` is on `PATH`.",
                "type": "string",
                "enum": [
                  "unix",
                  "windows"
                ]
              }
            },
            "additionalProperties": false,
            "required": [
              "name"
            ]
          },
          "description": "Multiple bootable systems for a dual-boot machine. Give one of `ssh`, `command`, `systems` or `via`."
        },
        "via": {
          "type": "string",
          "description": "Relay machine in `fleet.toml` through which calls to this machine are forwarded. Give one of `ssh`, `command`, `systems` or `via`."
        },
        "program": {
          "type": "string",
          "description": "Remote `legion` binary name or path when it is not found the default way."
        },
        "os": {
          "description": "Operating system hint (`\"unix\"` or `\"windows\"`).\n`unix`: Linux, macOS, or another POSIX-like host reached through `sh -c`.\n`windows`: Windows host where `legion` is on `PATH`.",
          "type": "string",
          "enum": [
            "unix",
            "windows"
          ]
        },
        "mac": {
          "type": "string",
          "description": "Hardware MAC address for waking the machine, such as `\"02:00:00:00:00:01\"`."
        },
        "wake_via": {
          "type": "array",
          "items": {
            "type": "string"
          },
          "description": "Helper machines that can wake this machine."
        }
      },
      "required": [
        "name"
      ]
    }
  },
  {
    "name": "fleet.discover",
    "description": "Discover peer machines from `tailscale status --json` using their MagicDNS names. Only peers belonging to this node's own Tailscale user (`Self.UserID`) are listed, and shared-in nodes (`ShareeNode`) are skipped so calls and `--on all` never reach machines owned by someone else. Tagged nodes are marked `addable: false` and skipped by `add: true` (add them explicitly with `fleet.add`). Pass `add: true` (requires admin permission) to add unlisted addable peers to fleet.toml.",
    "effect": "read",
    "input_schema": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "add": {
          "type": "boolean",
          "description": "Add every addable discovered machine (same Tailscale user, not shared in, not tagged) that\nis not already listed in fleet.toml. Requires admin permission.",
          "default": false
        }
      }
    }
  },
  {
    "name": "fleet.install",
    "description": "Install or upgrade the legion binary on a machine in fleet.toml using only ssh and scp. Refuses to overwrite a remote binary managed by a system package manager (such as `deb` or `pacman`) and reports the package manager's upgrade command instead. Detects the remote OS and architecture, verifies the offline Ed25519 signature (SHA256SUMS.sig) against the compiled-in release key and checks SHA-256 (for both downloaded and cached releases, with no skip flag; passing `from=<local file>` is the only way to install an unsigned binary, where the admin naming the file vouches for it), copies it atomically into place, and opens a normal session to verify the installed version.",
    "effect": "admin",
    "input_schema": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "machine": {
          "type": "string",
          "description": "Name of the machine in `fleet.toml` to install or upgrade `legion` on."
        },
        "version": {
          "type": "string",
          "description": "Release version to install (for example `0.2.0` or `legion-v0.2.0`). Defaults to the latest\npublished `legion-v*` release. Cannot be combined with `from`."
        },
        "from": {
          "type": "string",
          "description": "Local path on this node to a `legion` binary already built for the target machine."
        },
        "dry_run": {
          "type": "boolean",
          "description": "Probe the remote machine and report what would be copied where without modifying it.",
          "default": false
        },
        "allow_downgrade": {
          "type": "boolean",
          "description": "Permit installing a version older than the machine's current version or recorded version floor.",
          "default": false
        }
      },
      "required": [
        "machine"
      ]
    }
  },
  {
    "name": "fleet.list",
    "description": "List the machines configured in fleet.toml, their systems and transport targets, without making network connections.",
    "effect": "read",
    "input_schema": {
      "type": "object",
      "additionalProperties": false
    }
  },
  {
    "name": "fleet.ping",
    "description": "Check whether machines in fleet.toml answer hello, concurrently, reporting their node name, system, version, platform and latency.",
    "effect": "read",
    "input_schema": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "machines": {
          "type": "array",
          "items": {
            "type": "string"
          },
          "description": "Machine names to probe; every machine in fleet.toml when absent."
        },
        "timeout_s": {
          "type": "number",
          "description": "Seconds to wait per machine, up to 60; 8 when absent."
        }
      }
    }
  },
  {
    "name": "fleet.remove",
    "description": "Remove a machine entry from fleet.toml atomically while preserving comments and unrelated entries.",
    "effect": "admin",
    "input_schema": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "name": {
          "type": "string",
          "description": "Name of the machine to remove from fleet.toml."
        }
      },
      "required": [
        "name"
      ]
    }
  },
  {
    "name": "fleet.sync",
    "description": "Compare, push, or pull shared configuration documents (devices.toml, fleet.toml, policy.toml, projects.toml, and tools.d/*.toml) across machines in fleet.toml using per-peer BLAKE3 sync sidecars to detect conflicts. In status mode, reports same, changed_here, changed_there, both, only_here, or only_there without importing anything. Pushing or pulling devices.toml requires the owner role on the node doing the writing (for non-owner callers, fleet.sync syncs the other documents and reports devices.toml as skipped with the reason; propagation after local device tools runs node to node with the node's own authority). Paired devices (devices.toml) obey a strict one-way trust rule for live entries: push sends local entries to peers and imports ONLY tombstones back — never a peer's live devices. New or changed live devices from a peer are accepted only via an explicit `fleet.sync direction=pull from=<peer>` run by an owner, or when that peer pushes to this node with owner authority. Tombstones always win and may flow in every direction. Removal of a tools.d file is never propagated: push never deletes remote files, pull never deletes local files, and status reports files present on only one side as only_here or only_there. Syncing policy.toml is powerful because it changes who may do what on the receiving machine; every replacement is strictly validated by the receiver before installing, backed up, and recorded in the audit log with its hash before and after. Never syncs node.toml or tokens.toml.",
    "effect": "admin",
    "input_schema": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "machines": {
          "type": "array",
          "items": {
            "type": "string"
          },
          "description": "Machines to sync with; defaults to all machines in fleet.toml other than this node."
        },
        "documents": {
          "type": "array",
          "items": {
            "type": "string"
          },
          "description": "Shared documents to sync; defaults to devices.toml, fleet.toml, policy.toml, projects.toml, and any tools.d/*.toml files."
        },
        "direction": {
          "default": "status",
          "description": "`status` (default, compares hashes only), `push` (local -> remote), or `pull` (remote -> local).\n`status`: Compare BLAKE3 hashes without writing anything.\n`push`: Send local shared documents to remote machines when they have not diverged.\n`pull`: Fetch shared documents from one remote machine when local copies have not diverged.",
          "type": "string",
          "enum": [
            "status",
            "push",
            "pull"
          ]
        },
        "from": {
          "type": "string",
          "description": "Source machine to pull from when `direction` is `pull`."
        },
        "force": {
          "type": "boolean",
          "description": "Overwrite conflicts without failing (the receiver still validates every document strictly).",
          "default": false
        },
        "dry_run": {
          "type": "boolean",
          "description": "Report what would happen without modifying any configuration, backup, or sidecar files.",
          "default": false
        },
        "timeout_s": {
          "type": "number",
          "description": "Optional per-machine timeout in seconds (up to 300)."
        },
        "device_registry_push": {
          "type": "object",
          "additionalProperties": false,
          "properties": {
            "trigger": {
              "type": "string",
              "description": "Local device tool that triggered this push (`device.enrol`, `device.set`, or `device.remove`)."
            },
            "principal": {
              "type": "string",
              "description": "Principal that triggered the local device tool (or created the invite, for `device.enrol`)."
            },
            "device": {
              "type": "string",
              "description": "Paired device name that triggered the local device tool, when known."
            },
            "pairing_id": {
              "type": "string",
              "description": "Pairing identifier of the paired device that triggered the local device tool, when known."
            },
            "source_node": {
              "type": "string",
              "description": "Node name where the local device tool ran."
            }
          },
          "required": [
            "trigger"
          ],
          "description": "Optional audit metadata attached by this node during an automatic device-registry push after\n`device.enrol`, `device.set`, or `device.remove`. Valid only with `direction=\"push\"` and\n`documents=[\"devices.toml\"]` as owner."
        }
      }
    }
  },
  {
    "name": "fs.copy",
    "description": "Copy a file, symbolic link or directory tree without following symbolic links out of the tree.",
    "effect": "write",
    "input_schema": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "from": {
          "type": "string",
          "description": "Source file, symbolic link or directory (absolute or starting with `~`)."
        },
        "to": {
          "type": "string",
          "description": "Destination path (absolute or starting with `~`)."
        },
        "overwrite": {
          "type": "boolean",
          "description": "Overwrite existing files or merge into an existing directory at `to`.",
          "default": false
        },
        "recursive": {
          "type": "boolean",
          "description": "Copy directories and their contents recursively.",
          "default": false
        }
      },
      "required": [
        "from",
        "to"
      ]
    }
  },
  {
    "name": "fs.edit",
    "description": "Replace an exact string in a UTF-8 text file atomically while preserving line endings and permissions, with optional expect_hash conflict checking.",
    "effect": "write",
    "input_schema": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "path": {
          "type": "string",
          "description": "UTF-8 text file to edit (absolute or starting with `~`)."
        },
        "old": {
          "type": "string",
          "description": "Exact text to find and replace."
        },
        "new": {
          "type": "string",
          "description": "Replacement text."
        },
        "all": {
          "type": "boolean",
          "description": "Replace every match instead of requiring a single unambiguous match.",
          "default": false
        },
        "expect_hash": {
          "type": "string",
          "description": "Require the current file's BLAKE3 hash (`blake3:<hex>`) to match before editing."
        }
      },
      "required": [
        "path",
        "old",
        "new"
      ]
    }
  },
  {
    "name": "fs.find",
    "description": "Find files and directories under a path matching optional glob, kind and depth filters, respecting .gitignore and skipping hidden entries by default.",
    "effect": "read",
    "input_schema": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "path": {
          "type": "string",
          "description": "Root directory to search (absolute or starting with `~`)."
        },
        "glob": {
          "type": "string",
          "description": "Optional glob pattern (for example `\"**/*.rs\"` or `\"*.toml\"`)."
        },
        "kind": {
          "description": "Optional entry kind filter (`file`, `dir`, `symlink` or `other`).\n`file`: A regular file.\n`dir`: A directory.\n`symlink`: A symbolic link.\n`other`: Something else, such as a named pipe, socket or device node.",
          "type": "string",
          "enum": [
            "file",
            "dir",
            "symlink",
            "other"
          ]
        },
        "max_depth": {
          "type": "integer",
          "description": "Maximum directory depth to descend (1 means direct children only).",
          "minimum": 1
        },
        "limit": {
          "type": "integer",
          "default": 200,
          "minimum": 1,
          "description": "Maximum number of matches to return, from 1 to 5000.",
          "maximum": 5000
        },
        "ignore": {
          "type": "boolean",
          "description": "Respect `.gitignore` / `.ignore` rules and skip hidden files.",
          "default": true
        },
        "cross_filesystems": {
          "type": "boolean",
          "description": "Descend into directories mounted from other file systems (false stays on one file system).",
          "default": false
        }
      },
      "required": [
        "path"
      ]
    }
  },
  {
    "name": "fs.grep",
    "description": "Search text files for a regular expression with optional glob filter and context lines, skipping binary files and truncating very long lines.",
    "effect": "read",
    "input_schema": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "path": {
          "type": "string",
          "description": "File or directory to search (absolute or starting with `~`)."
        },
        "pattern": {
          "type": "string",
          "description": "Regular expression pattern to search for."
        },
        "glob": {
          "type": "string",
          "description": "Optional glob filter restricting which files are searched (for example `\"**/*.rs\"`)."
        },
        "ignore_case": {
          "type": "boolean",
          "description": "Match without distinguishing ASCII/Unicode letter case.",
          "default": false
        },
        "context": {
          "type": "integer",
          "default": 0,
          "minimum": 0,
          "description": "Number of surrounding lines to include before and after each match, from 0 to 5.",
          "maximum": 5
        },
        "limit": {
          "type": "integer",
          "default": 100,
          "minimum": 1,
          "description": "Maximum number of matches to return, from 1 to 1000.",
          "maximum": 1000
        },
        "ignore": {
          "type": "boolean",
          "description": "Respect `.gitignore` / `.ignore` rules and skip hidden files when searching a directory.",
          "default": true
        },
        "cross_filesystems": {
          "type": "boolean",
          "description": "Descend into directories mounted from other file systems (false stays on one file system).",
          "default": false
        }
      },
      "required": [
        "path",
        "pattern"
      ]
    }
  },
  {
    "name": "fs.hash",
    "description": "Stream a file and return its size and BLAKE3 digest as blake3:<hex>.",
    "effect": "read",
    "input_schema": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "path": {
          "type": "string",
          "description": "File to hash (absolute or starting with `~`)."
        }
      },
      "required": [
        "path"
      ]
    }
  },
  {
    "name": "fs.list",
    "description": "List entries in a directory up to a bounded depth and count, directories first then by name, reporting symbolic links without following them.",
    "effect": "read",
    "input_schema": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "path": {
          "type": "string",
          "description": "Directory to list (absolute or starting with `~`)."
        },
        "depth": {
          "type": "integer",
          "default": 1,
          "minimum": 1,
          "description": "How many directory levels to descend, from 1 to 8.",
          "maximum": 8
        },
        "hidden": {
          "type": "boolean",
          "description": "Include hidden files and directories (dotfiles, and Windows hidden entries).",
          "default": true
        },
        "limit": {
          "type": "integer",
          "default": 500,
          "minimum": 1,
          "description": "Maximum number of entries to return, from 1 to 5000.",
          "maximum": 5000
        }
      },
      "required": [
        "path"
      ]
    }
  },
  {
    "name": "fs.manifest",
    "description": "List a directory tree (or one file) the way fs.transfer compares it: each entry's relative path, kind, size, modification time, mode, link target and, with checksum, its BLAKE3 hash. Bounded by limit.",
    "effect": "read",
    "input_schema": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "path": {
          "type": "string",
          "description": "Directory or file to list (absolute or starting with `~`)."
        },
        "checksum": {
          "type": "boolean",
          "description": "Hash every regular file with BLAKE3, which reads all of them.",
          "default": false
        },
        "exclude": {
          "type": "array",
          "items": {
            "type": "string"
          },
          "description": "Globs of paths to leave out, relative to `path`; a trailing `/` matches directories only.",
          "default": []
        },
        "limit": {
          "type": "integer",
          "default": 1000,
          "minimum": 1,
          "description": "Most entries to return, from 1 to 10000.",
          "maximum": 10000
        }
      },
      "required": [
        "path"
      ]
    }
  },
  {
    "name": "fs.mkdir",
    "description": "Create a directory and, by default, any missing parent directories.",
    "effect": "write",
    "input_schema": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "path": {
          "type": "string",
          "description": "Directory to create (absolute or starting with `~`)."
        },
        "parents": {
          "type": "boolean",
          "description": "Create missing parent directories and succeed if the directory already exists.",
          "default": true
        }
      },
      "required": [
        "path"
      ]
    }
  },
  {
    "name": "fs.move",
    "description": "Move or rename a file, symbolic link or directory, falling back to copy-then-remove across file systems.",
    "effect": "write",
    "input_schema": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "from": {
          "type": "string",
          "description": "Existing path to move (absolute or starting with `~`)."
        },
        "to": {
          "type": "string",
          "description": "Destination path (absolute or starting with `~`)."
        },
        "overwrite": {
          "type": "boolean",
          "description": "Replace an existing file, symbolic link or empty directory at `to`.",
          "default": false
        }
      },
      "required": [
        "from",
        "to"
      ]
    }
  },
  {
    "name": "fs.read",
    "description": "Read up to 1 MiB of a file from an offset as UTF-8 or base64, returning the whole-file BLAKE3 hash when the entire file fit in this reply.",
    "effect": "read",
    "input_schema": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "path": {
          "type": "string",
          "description": "File to read (absolute or starting with `~`)."
        },
        "offset": {
          "type": "integer",
          "default": 0,
          "minimum": 0,
          "description": "Byte offset to start reading from."
        },
        "length": {
          "type": "integer",
          "default": 1048576,
          "minimum": 0,
          "description": "Maximum number of bytes to read, up to 1 MiB (1048576).",
          "maximum": 1048576
        },
        "encoding": {
          "description": "Output encoding (`utf8` or `base64`); omitted selects `utf8` when valid and `base64`\notherwise.\n`utf8`: UTF-8 text.\n`base64`: Standard base64-encoded bytes.",
          "type": "string",
          "enum": [
            "utf8",
            "base64"
          ]
        }
      },
      "required": [
        "path"
      ]
    }
  },
  {
    "name": "fs.remove",
    "description": "Remove a file, symbolic link or directory (with optional recursive and dry_run modes), refusing roots, the home directory and Legion's own directories.",
    "effect": "destroy",
    "input_schema": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "path": {
          "type": "string",
          "description": "Path to remove (absolute or starting with `~`)."
        },
        "recursive": {
          "type": "boolean",
          "description": "Remove a directory and everything inside it.",
          "default": false
        },
        "dry_run": {
          "type": "boolean",
          "description": "Report what would be removed without changing the file system.",
          "default": false
        }
      },
      "required": [
        "path"
      ]
    }
  },
  {
    "name": "fs.stat",
    "description": "Return metadata for a path (kind, size, modification time, Unix mode, readonly flag, and symbolic link target) without following a symbolic link.",
    "effect": "read",
    "input_schema": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "path": {
          "type": "string",
          "description": "Path to inspect (absolute or starting with `~`)."
        }
      },
      "required": [
        "path"
      ]
    }
  },
  {
    "name": "fs.transfer",
    "description": "Copy a file or a directory tree between machines, or within one. Each of from and to is machine:/path or a path on this node; to names the destination itself, except that a file sent to an existing directory (or to a path ending in /) lands inside it under its own name. A file resumes where an interrupted transfer stopped and is renamed into place only once its BLAKE3 hash matches. A tree sends only what differs (size and modification time, or hash with checksum), so a second run sends nothing; delete removes what the source no longer has, exclude leaves paths out at both ends, and dry_run reports what would be sent and deleted without changing anything.",
    "effect": "write",
    "input_schema": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "from": {
          "type": "string",
          "description": "What to send: `machine:/path`, or a path on this node (absolute or starting with `~`)."
        },
        "to": {
          "type": "string",
          "description": "Where it goes, in the same form. It names the destination itself; a file sent to an\nexisting directory lands inside it."
        },
        "delete": {
          "type": "boolean",
          "description": "Remove from the destination tree what the source no longer has.",
          "default": false
        },
        "checksum": {
          "type": "boolean",
          "description": "Compare files by BLAKE3 hash instead of size and modification time, which reads them all.",
          "default": false
        },
        "exclude": {
          "type": "array",
          "items": {
            "type": "string"
          },
          "description": "Globs of paths to leave out at both ends, relative to the tree; a trailing `/` matches\ndirectories only. What is left out at the destination is never deleted.",
          "default": []
        },
        "dry_run": {
          "type": "boolean",
          "description": "List and plan, and report what would be sent and deleted, without changing anything.",
          "default": false
        }
      },
      "required": [
        "from",
        "to"
      ]
    }
  },
  {
    "name": "fs.write",
    "description": "Write UTF-8 text or base64 bytes to a file atomically, supporting overwrite, create-only and append modes, optional expect_hash conflict checks, parent creation and Unix mode.",
    "effect": "write",
    "input_schema": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "path": {
          "type": "string",
          "description": "File to write (absolute or starting with `~`)."
        },
        "content": {
          "type": "string",
          "description": "Content to write, as UTF-8 text or standard base64 according to `encoding`."
        },
        "encoding": {
          "default": "utf8",
          "description": "Encoding of `content` (`utf8` or `base64`, default `utf8`).\n`utf8`: UTF-8 text.\n`base64`: Standard base64-encoded bytes.",
          "type": "string",
          "enum": [
            "utf8",
            "base64"
          ]
        },
        "mode": {
          "description": "Write mode (`overwrite`, `create` or `append`, default `overwrite`).\n`overwrite`: Create the file or atomically replace its contents.\n`create`: Create a new file and fail with `exists` if anything is already at the path.\n`append`: Append to an existing file in OS append mode or create it if missing.",
          "type": "string",
          "enum": [
            "overwrite",
            "create",
            "append"
          ]
        },
        "expect_hash": {
          "type": "string",
          "description": "Require the existing file's BLAKE3 hash (`blake3:<hex>`) to match before writing."
        },
        "parents": {
          "type": "boolean",
          "description": "Create missing parent directories before writing.",
          "default": false
        },
        "unix_mode": {
          "type": "string",
          "description": "Unix permissions as an octal string, such as `\"0600\"` or `\"0644\"`. Ignored on Windows."
        }
      },
      "required": [
        "path",
        "content"
      ]
    }
  },
  {
    "name": "job.cancel",
    "description": "Stop a running background job and its entire process tree. Asks the job supervisor to terminate the command cleanly and escalates to stopping the detached process group or systemd scope if it does not finish within a short grace period; set `force: true` to stop the detached handle immediately. Returns only once the job is gone or reports any surviving process ids. Any principal with the exec effect can cancel any job on this node. Cancelling a job that already finished is not an error and returns its final state.",
    "effect": "exec",
    "input_schema": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "id": {
          "type": "string",
          "description": "The job identifier returned by `job.start` or `job.list`."
        },
        "force": {
          "type": "boolean",
          "description": "Skip waiting for the supervisor to cancel cooperatively and terminate the detached process\ntree or systemd scope immediately.",
          "default": false
        }
      },
      "required": [
        "id"
      ]
    }
  },
  {
    "name": "job.list",
    "description": "List background jobs recorded on this node, newest first, with their id, name, current derived state, timestamps, exit code, and command summary. Filter by `state` to find running, failed, or interrupted jobs. Any principal with the read effect sees every job's command and output on this node. Reading never modifies job files.",
    "effect": "read",
    "input_schema": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "state": {
          "description": "Keep only jobs currently in this derived state.\n`starting`: The job record is written and the detached supervisor is starting up.\n`running`: The supervisor holds the job's kernel lock and is running the command.\n`succeeded`: The command exited with code 0.\n`failed`: The command exited with a non-zero code, ended on a signal, or failed to start.\n`cancelled`: The job was stopped via `job.cancel`.\n`interrupted`: No exit record exists and the supervisor's lock is free (the supervisor died or the machine\nrebooted).",
          "type": "string",
          "enum": [
            "starting",
            "running",
            "succeeded",
            "failed",
            "cancelled",
            "interrupted"
          ]
        },
        "limit": {
          "type": "integer",
          "maximum": 500,
          "minimum": 1,
          "description": "Maximum number of jobs to return, 1 to 500; 50 when absent."
        }
      }
    }
  },
  {
    "name": "job.logs",
    "description": "Read captured stdout and stderr from a background job without loading the whole log into memory. Stdout and stderr are merged into one log: each stream stays in its own order and lines (up to 64 KiB) are written whole under one lock so the two streams never split a line mid-way, while relative order between stdout and stderr is unspecified. Pass `offset` (feeding back `next_offset` from each reply) to stream new output incrementally while the job runs, or omit `offset` to read the last `tail` lines (default 100, max 5000). Replies are capped at `limit` bytes (default 65536, max 1048576), never split a UTF-8 character, and replace invalid UTF-8 bytes. Large logs rotate once into `output.log.1` and are read across both segments transparently; `output_dropped` and `dropped_bytes` report when earlier output was discarded by rotation. Any principal with the read effect sees every job's command and output on this node.",
    "effect": "read",
    "input_schema": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "id": {
          "type": "string",
          "description": "The job identifier returned by `job.start` or `job.list`."
        },
        "offset": {
          "type": "integer",
          "description": "Byte offset to read forward from (feed back `next_offset` to poll incrementally). Omit to\nread the last `tail` lines instead.",
          "minimum": 0
        },
        "tail": {
          "type": "integer",
          "maximum": 5000,
          "minimum": 1,
          "description": "Number of trailing lines to read when `offset` is omitted (1 to 5000; 100 by default)."
        },
        "limit": {
          "type": "integer",
          "maximum": 1048576,
          "minimum": 1,
          "description": "Maximum bytes to return in one reply (1 to 1048576; 65536 by default)."
        }
      },
      "required": [
        "id"
      ]
    }
  },
  {
    "name": "job.start",
    "description": "Start a command as a detached background job that outlives this call, this legion process, and any ssh session. Give `argv` to run a program with exact arguments or `cmd` for the platform shell (sh on Linux and macOS, PowerShell on Windows). Returns as soon as the job supervisor holds its lock; afterwards poll `job.logs` with `next_offset` to read output as it arrives, call `job.wait` to wait for completion, or inspect `job.status`. Pass `key` to make retries after a dropped connection idempotent: a key matches an active job or one whose supervisor took the lock and ran (succeeded, failed, cancelled, or interrupted mid-run); a failed start whose supervisor never took the lock does not match and is replaced. Set `exclusive: true` when the job should mark this machine busy.",
    "effect": "exec",
    "input_schema": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "cmd": {
          "type": "string",
          "description": "A command line handed whole to the platform shell: sh on Linux and macOS, PowerShell on\nWindows. Give this or `argv`, not both."
        },
        "argv": {
          "type": "array",
          "items": {
            "type": "string"
          },
          "description": "The program and its arguments, run exactly as given with no shell in between. Give this\nor `cmd`, not both."
        },
        "cwd": {
          "type": "string",
          "description": "The absolute directory to run in (accepts `~`); legion's own when absent."
        },
        "env": {
          "type": "object",
          "additionalProperties": {
            "type": "string"
          },
          "description": "Environment variables to set on top of legion's own. Values are stored in a private file\nand never included in job records or replies."
        },
        "name": {
          "type": "string",
          "description": "Short human-readable name for the job; defaults to the command's program name."
        },
        "key": {
          "type": "string",
          "description": "Caller-chosen idempotency key. Matches an active job or one whose supervisor took the lock\nand ran (succeeded, failed, cancelled, or interrupted mid-run): re-calling with the same\nkey and identical command, `cwd`, and environment variable names returns that job instead\nof starting a second one, while differing arguments fail with `conflict`. A failed start\nwhose supervisor never took the lock does not match and is replaced."
        },
        "exclusive": {
          "type": "boolean",
          "description": "Mark this machine busy while the job runs (blocks unattended `power.*` operations).",
          "default": false
        }
      }
    }
  },
  {
    "name": "job.status",
    "description": "Inspect one background job's full record (without environment values), current derived state, exit code or signal, duration, output size, and failure or interruption detail without modifying anything on disk. Pass an `id` from `job.start` or `job.list`; use `job.logs` to read output or `job.wait` to wait until it finishes.",
    "effect": "read",
    "input_schema": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "id": {
          "type": "string",
          "description": "The job identifier returned by `job.start` or `job.list`."
        }
      },
      "required": [
        "id"
      ]
    }
  },
  {
    "name": "job.wait",
    "description": "Wait until a background job is no longer starting or running and return the same status object as `job.status`, plus `timed_out`. Returns as soon as the job reaches a terminal state, or returns with `timed_out: true` when `timeout_s` elapses (default 60, max 3600). Stops waiting immediately if this call is cancelled. Use this after `job.start` instead of polling `job.status` in a tight loop.",
    "effect": "read",
    "input_schema": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "id": {
          "type": "string",
          "description": "The job identifier returned by `job.start` or `job.list`."
        },
        "timeout_s": {
          "type": "number",
          "description": "Seconds to wait before returning with `timed_out: true` (up to 3600; 60 when absent)."
        }
      },
      "required": [
        "id"
      ]
    }
  },
  {
    "name": "meta.doctor",
    "description": "Check that this node is healthy: its configuration directory can be read, its state directory exists and can be written to, every configuration file parses, how legion was installed, the executable directory and ssh are on PATH, Linux systemd lingering is enabled, non-owner roles that allow exec are flagged as a warning, and each machine in fleet.toml is checked (unless quick=true; unreachable machines are reported as warnings while protocol or reply errors fail). Each check says what it found and, when something is wrong, what to do. Passing acknowledge=\"devices-rollback\" (owner role only) clears a recorded configuration rollback failure in the state directory and audits who acknowledged it; otherwise it writes nothing but a probe file in the state directory, which it removes.",
    "effect": "read",
    "input_schema": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "quick": {
          "type": "boolean",
          "description": "Skip checking whether remote machines in fleet.toml answer.",
          "default": false
        },
        "acknowledge": {
          "type": "string",
          "description": "Acknowledge and clear a recorded health failure (owner role only). Pass\n`devices-rollback` after verifying policy.toml and devices.toml to clear\n`state/devices-rollback-error.json` and audit who acknowledged it."
        }
      }
    }
  },
  {
    "name": "meta.init",
    "description": "Create starter node.toml and policy.toml files where they do not already exist, leaving every existing file untouched so it is safe to run again.",
    "effect": "admin",
    "input_schema": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "name": {
          "type": "string",
          "description": "Node name to write into node.toml when creating it; defaults to the name derived from this machine's host name."
        },
        "roles": {
          "type": "boolean",
          "description": "Create policy.toml with ready-made observer, editor and operator roles when missing.",
          "default": true
        },
        "dry_run": {
          "type": "boolean",
          "description": "Report what would be created or kept without writing any files.",
          "default": false
        }
      }
    }
  },
  {
    "name": "meta.update",
    "description": "Replace this node's legion binary with an offline-signed GitHub release artifact or a local binary passed with `from`. Refuses installations managed by a system package manager (such as `deb` or `pacman`) and points at the package manager's upgrade command instead. Release downloads require a valid Ed25519 SHA256SUMS.sig matching the public key compiled into legion (no flag can bypass it) and SHA-256 verification; passing `from=<local file>` installs an unsigned binary that the admin naming the file vouches for. Checks binary format and architecture, enforces downgrade protection unless `allow_downgrade` is set, stages and runs `--version` preflight, keeps the previous binary as `.prev` (restoring it if the replaced binary does not run), and replaces atomically. On Windows, renames the running executable aside first so it can be cleaned up on the next start.",
    "effect": "admin",
    "input_schema": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "version": {
          "type": "string",
          "description": "Release version to install (for example `0.2.0` or `legion-v0.2.0`). Defaults to the\nhighest published stable `legion-v*` semantic version. Cannot be combined with `from`."
        },
        "from": {
          "type": "string",
          "description": "Local path on this node to a `legion` binary already built for this machine. Cannot be\ncombined with `version`."
        },
        "dry_run": {
          "type": "boolean",
          "description": "Resolve the target release or local binary and report what would be replaced without\ndownloading or writing.",
          "default": false
        },
        "allow_downgrade": {
          "type": "boolean",
          "description": "Permit replacing with a version older than the running binary or the recorded version floor.",
          "default": false
        }
      }
    }
  },
  {
    "name": "meta.version",
    "description": "Say which build of legion this node runs, how it was installed, the protocol it speaks, and its operating system and architecture.",
    "effect": "read",
    "input_schema": {
      "type": "object",
      "additionalProperties": false
    }
  },
  {
    "name": "meta.whoami",
    "description": "Say who the caller is on this node: the principal and role it acts as, the door it came through, the directories file tools are confined to, and the tools it may use.",
    "effect": "read",
    "input_schema": {
      "type": "object",
      "additionalProperties": false
    }
  },
  {
    "name": "power.cancel",
    "description": "Call off the sleep, restart or shutdown handed over on this machine and not yet carried out: on Linux and macOS the delayed runner is stopped before it runs the command, on Windows a restart or shutdown is called off with `shutdown /a`, and a one-time firmware boot entry the restart had armed is cleared again. power.status then shows the action as `last`, cancelled, with who cancelled it. `cancelled: false` with a `detail` when nothing is pending, or when the action has already started and can no longer be stopped. Not gated on the machine being busy.",
    "effect": "power",
    "input_schema": {
      "type": "object",
      "additionalProperties": false
    }
  },
  {
    "name": "power.restart",
    "description": "Restart this machine `delay_s` seconds (1 to 600, default 3) after this call replies, optionally into another of its systems: `into` names a boot target from node.toml ([boot.<name>]), the firmware is set to start it once and read back, and only when the setting reads back is the restart scheduled; otherwise nothing restarts. The reply means the restart was scheduled, not that it happened, and carries the current boot id to compare with later; until it happens, power.cancel calls it off. To restart another machine and confirm which system came back, call power.switch from a different machine. Busy gate: the call fails with `busy` while a busy probe or an exclusive job says the machine is busy, or while node.toml says nothing about what keeps it busy (`unmonitored`; the error says what to write); `force: true` goes ahead anyway. Without it the state is checked again just before acting: on Linux and macOS also when the delay has passed, so a machine that became busy meanwhile is not interrupted and `last` says so; on Windows, whose own timer carries out a restart or shutdown, only when it is handed over. On Linux a logind inhibitor (an application asking not to be interrupted) also fails it with `busy`, naming who holds it; `force` does not override that, `ignore_inhibitors: true` does. One power action runs at a time: while one is pending the call fails with `conflict`. `dry_run: true` returns the exact commands without running them, and when a check would refuse the real call, what it would say (`would_refuse`).",
    "effect": "power",
    "input_schema": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "into": {
          "type": "string",
          "description": "A boot target from this machine's node.toml ([boot.<name>]) to start once, such as\n`windows`; power.status lists them. Absent restarts into the firmware's usual choice."
        },
        "force": {
          "type": "boolean",
          "description": "Go ahead even while the machine is busy or unmonitored. It does not override a logind\ninhibitor; `ignore_inhibitors` does.",
          "default": false
        },
        "ignore_inhibitors": {
          "type": "boolean",
          "description": "Linux: go ahead even while a logind inhibitor (an application asking not to be\ninterrupted, such as a running update or an unsaved document) holds the action back,\nthrough systemctl's own `-i`. It needs `[power] sudo = true`. No effect elsewhere.",
          "default": false
        },
        "delay_s": {
          "type": "integer",
          "maximum": 600,
          "minimum": 1,
          "description": "Seconds between the reply and the restart, 1 to 600; 3 when absent."
        },
        "dry_run": {
          "type": "boolean",
          "description": "Return the exact commands this call would run, without running them.",
          "default": false
        }
      }
    }
  },
  {
    "name": "power.shutdown",
    "description": "Power this machine off `delay_s` seconds (1 to 600, default 3) after this call replies. The reply means the shutdown was scheduled, not that it happened; until it happens, power.cancel calls it off. Bringing the machine back takes Wake-on-LAN (power.wake from another machine) or someone at it. Busy gate: the call fails with `busy` while a busy probe or an exclusive job says the machine is busy, or while node.toml says nothing about what keeps it busy (`unmonitored`; the error says what to write); `force: true` goes ahead anyway. Without it the state is checked again just before acting: on Linux and macOS also when the delay has passed, so a machine that became busy meanwhile is not interrupted and `last` says so; on Windows, whose own timer carries out a restart or shutdown, only when it is handed over. On Linux a logind inhibitor (an application asking not to be interrupted) also fails it with `busy`, naming who holds it; `force` does not override that, `ignore_inhibitors: true` does. One power action runs at a time: while one is pending the call fails with `conflict`. `dry_run: true` returns the exact commands without running them, and when a check would refuse the real call, what it would say (`would_refuse`).",
    "effect": "power",
    "input_schema": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "force": {
          "type": "boolean",
          "description": "Go ahead even while the machine is busy or unmonitored. It does not override a logind\ninhibitor; `ignore_inhibitors` does.",
          "default": false
        },
        "ignore_inhibitors": {
          "type": "boolean",
          "description": "Linux: go ahead even while a logind inhibitor (an application asking not to be\ninterrupted, such as a running update or an unsaved document) holds the action back,\nthrough systemctl's own `-i`. It needs `[power] sudo = true`. No effect elsewhere.",
          "default": false
        },
        "delay_s": {
          "type": "integer",
          "maximum": 600,
          "minimum": 1,
          "description": "Seconds between the reply and the shutdown, 1 to 600; 3 when absent."
        },
        "dry_run": {
          "type": "boolean",
          "description": "Return the exact commands this call would run, without running them.",
          "default": false
        }
      }
    }
  },
  {
    "name": "power.sleep",
    "description": "Put this machine to sleep about 3 seconds after this call replies. The reply means the sleep was handed to the operating system, not that the machine is asleep. The commands in node.toml's [power] before_sleep run first, and a failure there stops the sleep. Busy gate: the call fails with `busy` while a busy probe or an exclusive job says the machine is busy, or while node.toml says nothing about what keeps it busy (`unmonitored`; the error says what to write); `force: true` goes ahead anyway. Without it the state is checked again just before acting: on Linux and macOS also when the delay has passed, so a machine that became busy meanwhile is not interrupted and `last` says so; on Windows, whose own timer carries out a restart or shutdown, only when it is handed over. On Linux a logind inhibitor (an application asking not to be interrupted) also fails it with `busy`, naming who holds it; `force` does not override that, `ignore_inhibitors: true` does. One power action runs at a time: while one is pending the call fails with `conflict`. `dry_run: true` returns the exact commands without running them, and when a check would refuse the real call, what it would say (`would_refuse`).",
    "effect": "power",
    "input_schema": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "force": {
          "type": "boolean",
          "description": "Go ahead even while the machine is busy or unmonitored. It does not override a logind\ninhibitor; `ignore_inhibitors` does.",
          "default": false
        },
        "ignore_inhibitors": {
          "type": "boolean",
          "description": "Linux: go ahead even while a logind inhibitor (an application asking not to be\ninterrupted, such as a running update or an unsaved document) holds the action back,\nthrough systemctl's own `-i`. It needs `[power] sudo = true`. No effect elsewhere.",
          "default": false
        },
        "dry_run": {
          "type": "boolean",
          "description": "Return the exact commands this call would run, without running them.",
          "default": false
        }
      }
    }
  },
  {
    "name": "power.status",
    "description": "Report this machine's power state without changing anything: the sleep, restart or shutdown handed over and not yet carried out (`pending`, which power.cancel calls off) and how the last one ended (`last`); whether it is busy (`idle`; `busy` with the reasons, each from a busy probe or an exclusive job; or `unmonitored` when node.toml says nothing about what keeps it busy), which system of the machine this is, its boot id (which changes on every restart), the boot targets power.restart can name in `into`, and whether system actions run through sudo. Busy probes run concurrently, each within its own timeout.",
    "effect": "read",
    "input_schema": {
      "type": "object",
      "additionalProperties": false
    }
  },
  {
    "name": "power.switch",
    "description": "Restart another machine, optionally into another of its systems, and confirm it came back. Reads the machine's boot id and system, calls power.restart on it (with `into`, a boot target in that system's node.toml, `force` and `ignore_inhibitors`; the machine's own busy gate applies, and a `busy` or `conflict` refusal comes back as is), then looks at each of the machine's systems every few seconds for up to `wait_s` seconds (0 to 900, default 180) until one answers with a different boot id. `restarted` is true only then. When `into` was given, the system that answers must be that one, or the call fails with `failed` and says which system came up instead. The connection dropping while the machine goes down is expected. With wait_s 0 the restart is only requested and `restarted` is false. `dry_run: true` returns the machine's own power.restart dry run (`would_run`, with the exact commands) and restarts nothing. Call it from a machine other than the one being restarted.",
    "effect": "power",
    "input_schema": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "machine": {
          "type": "string",
          "description": "The machine to restart, as fleet.toml names it."
        },
        "into": {
          "type": "string",
          "description": "The system to restart into: a boot target in node.toml of the system running now, such as\n`windows`, which must also be the `system` the other one's node.toml names. Absent restarts\ninto the firmware's usual choice."
        },
        "wait_s": {
          "type": "integer",
          "maximum": 900,
          "minimum": 0,
          "description": "Seconds to wait for the machine to come back, 0 to 900; 180 when absent. 0 requests the\nrestart and returns."
        },
        "force": {
          "type": "boolean",
          "description": "Restart even while the machine is busy or unmonitored. It does not override a logind\ninhibitor there; `ignore_inhibitors` does.",
          "default": false
        },
        "ignore_inhibitors": {
          "type": "boolean",
          "description": "Restart even while a logind inhibitor holds it back (Linux, with `[power] sudo = true`\nthere).",
          "default": false
        },
        "dry_run": {
          "type": "boolean",
          "description": "Ask the machine for its power.restart dry run: the exact commands it would run, run nowhere.",
          "default": false
        }
      },
      "required": [
        "machine"
      ]
    }
  },
  {
    "name": "power.wake",
    "description": "Wake a machine from fleet.toml. If it already answers, nothing is sent and `awake` is true. Otherwise each machine in its `wake_via` list is tried in order, but only one that answers right now: a helper that does not answer is skipped and never woken itself. The first that answers is asked to run power.wol for the machine, using the hardware address in the helper's own fleet.toml; with no wake_via, this machine sends the packet itself. `sent_by` names who sent it. With `wait_s` (0 to 300, default 0) the call then waits for the machine itself to answer, and `awake` is true only once it has. A packet sent is not a machine awake. `dry_run: true` chooses the sender the same way and returns where its packets would go (`would_send`) without sending any.",
    "effect": "power",
    "input_schema": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "machine": {
          "type": "string",
          "description": "The machine to wake, as fleet.toml names it."
        },
        "wait_s": {
          "type": "integer",
          "default": 0,
          "minimum": 0,
          "description": "Seconds to wait for the machine itself to answer after the packet was sent, 0 to 300; 0\n(send and return) when absent.",
          "maximum": 300
        },
        "dry_run": {
          "type": "boolean",
          "description": "Choose the sender as a real call would, and return where its packets would go without\nsending any.",
          "default": false
        }
      },
      "required": [
        "machine"
      ]
    }
  },
  {
    "name": "power.wol",
    "description": "Send Wake-on-LAN magic packets from this machine for a machine named in THIS node's fleet.toml, taking its hardware address (`mac`) from there, never from the call: to the broadcast address of each connected network interface and to 255.255.255.255, port 9, three times each. Wake packets only travel within the local network, so this wakes machines on the same network as this one. The reply says how many packets left; it never says the machine woke, which only the machine answering can show (power.wake waits for that). `dry_run: true` lists where the packets would go without sending any.",
    "effect": "power",
    "input_schema": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "machine": {
          "type": "string",
          "description": "The machine to wake, as this node's fleet.toml names it; its hardware address is read from\nthere."
        },
        "dry_run": {
          "type": "boolean",
          "description": "List where the packets would go without sending any.",
          "default": false
        }
      },
      "required": [
        "machine"
      ]
    }
  },
  {
    "name": "proj.add",
    "description": "Record a project in projects.toml, or change one. With `path`, records where it lives on this machine; the directory must exist unless the project has a `repo` that proj.clone can fetch into it. Paths the project has on other machines are left as they are. `repo` and `description` are set when given. Comments and everything else in the file are kept.",
    "effect": "write",
    "input_schema": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "name": {
          "type": "string",
          "description": "The project's name: letters, digits, `-`, `_` and `.`, starting with a letter or digit."
        },
        "path": {
          "type": "string",
          "description": "Where it lives on this machine, absolute or starting at `~`."
        },
        "repo": {
          "type": "string",
          "description": "Where proj.clone fetches it from: any URL or path `git clone` accepts."
        },
        "description": {
          "type": "string",
          "description": "One line saying what it is."
        }
      },
      "required": [
        "name"
      ]
    }
  },
  {
    "name": "proj.clone",
    "description": "Clone a project's `repo` onto this machine and record where it went. The destination is `path` when given, else the path already recorded for this machine, else ~/projects/<name>; it must not exist or must be an empty directory. Reports git's progress as progress events and returns the commit checked out. Git never prompts: a repository that needs credentials git cannot find on its own fails instead of waiting. Git may run hooks and helpers (credential helpers, ssh) that the repository or the user configured.",
    "effect": "exec",
    "input_schema": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "project": {
          "type": "string",
          "description": "The project to clone; it must have a `repo`."
        },
        "path": {
          "type": "string",
          "description": "Where to clone it, absolute or starting at `~`; the path recorded for this machine, or\n~/projects/<name>, when absent."
        }
      },
      "required": [
        "project"
      ]
    }
  },
  {
    "name": "proj.discover",
    "description": "Find git working trees under `root`, down to `max_depth` levels (3 unless given, at most 6), without descending into a working tree, hidden directories or dependency and build directories such as node_modules and target. Each one found has a suggested project name, its origin remote and whether projects.toml already lists it here. With `add: true` the ones not yet listed are recorded with their paths on this machine, which needs permission to use proj.add.",
    "effect": "read",
    "input_schema": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "root": {
          "type": "string",
          "description": "The directory to search, absolute or starting at `~`."
        },
        "max_depth": {
          "type": "integer",
          "maximum": 6,
          "minimum": 0,
          "description": "How many levels below `root` to look, 0 to 6; 3 when absent."
        },
        "add": {
          "type": "boolean",
          "description": "Record the working trees not yet listed as projects living on this machine.",
          "default": false
        }
      },
      "required": [
        "root"
      ]
    }
  },
  {
    "name": "proj.list",
    "description": "List the projects recorded in projects.toml: each one's name, description, repo, the path it has on this machine and whether that path exists, the other machines it lives on, and the names of its tasks. Every other proj tool takes a project by the name shown here. Use proj.status to see the git state of the ones that live here, and `on` to ask another machine.",
    "effect": "read",
    "input_schema": {
      "type": "object",
      "additionalProperties": false
    }
  },
  {
    "name": "proj.pull",
    "description": "Update a project on this machine from its upstream with `git pull --ff-only`, which only ever moves the branch forward. When the branches have diverged or local changes are in the way, nothing changes and the call fails with `conflict`, carrying git's own explanation in the hint. Returns the commit before and after and a summary of what changed. Git may run hooks and helpers (credential helpers, ssh) that the repository or the user configured.",
    "effect": "exec",
    "input_schema": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "project": {
          "type": "string",
          "description": "The project to update."
        }
      },
      "required": [
        "project"
      ]
    }
  },
  {
    "name": "proj.remove",
    "description": "Forget where a project lives on this machine, or with `everywhere: true` forget the whole project on every machine. Only projects.toml changes: no file of the project is ever deleted.",
    "effect": "write",
    "input_schema": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "name": {
          "type": "string",
          "description": "The project's name."
        },
        "everywhere": {
          "type": "boolean",
          "description": "Forget the whole project on every machine, not just where it lives on this one.",
          "default": false
        }
      },
      "required": [
        "name"
      ]
    }
  },
  {
    "name": "proj.run",
    "description": "Run one of a project's tasks (see proj.list) in its directory on this machine. `args` are added to the task's command as separate arguments, never spliced into a command line. A plain task runs to the end and returns what exec.run returns (exit code, output, whether it timed out), with output streamed as events; it is stopped after its `timeout_s`, 600 seconds unless the task says. A task marked `job = true` starts as a background job and returns its id at once: follow it with job.logs and job.wait.",
    "effect": "exec",
    "input_schema": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "project": {
          "type": "string",
          "description": "The project whose task to run."
        },
        "task": {
          "type": "string",
          "description": "The task's name, as proj.list shows it."
        },
        "args": {
          "type": "array",
          "items": {
            "type": "string"
          },
          "description": "Arguments added after the task's command, each passed as one argument.",
          "default": []
        }
      },
      "required": [
        "project",
        "task"
      ]
    }
  },
  {
    "name": "proj.status",
    "description": "Show the git state of a project on this machine, or of every project that lives here when `project` is left out: branch, commit, upstream and how far ahead or behind it is (as of the last fetch), how many files are staged, changed, untracked or in conflict, the last commit and the stash. A project whose directory is not a git repository has no `git`; one that cannot be read has an `error`, and never stops the others from being reported. Nothing is fetched and nothing is changed.",
    "effect": "read",
    "input_schema": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "project": {
          "type": "string",
          "description": "The project to look at; every project that lives on this machine when absent."
        }
      }
    }
  },
  {
    "name": "svc.list",
    "description": "List background services managed by systemd (Linux), launchd (macOS) or the Windows Service Control Manager. `scope` selects \"user\" (per-user services), \"system\" (machine-wide services; the only scope on Windows) or \"all\" (default). Read-only and never escalates privileges.",
    "effect": "read",
    "input_schema": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "scope": {
          "description": "Which scope to list: `\"user\"`, `\"system\"`, or `\"all\"` (default `\"all\"`). On Windows only\n`\"system\"` exists.\n`user`: Only per-user services.\n`system`: Only machine-wide services.\n`all`: Both user and system services (where both exist).",
          "type": "string",
          "enum": [
            "user",
            "system",
            "all"
          ]
        },
        "filter": {
          "type": "string",
          "description": "Keep only services whose name or description contains this substring (case-insensitive)."
        },
        "state": {
          "description": "Keep only services in this state: `\"running\"`, `\"stopped\"` or `\"failed\"`.\n`running`: Keep only running services.\n`stopped`: Keep only stopped services.\n`failed`: Keep only failed services.",
          "type": "string",
          "enum": [
            "running",
            "stopped",
            "failed"
          ]
        },
        "limit": {
          "type": "integer",
          "default": 100,
          "minimum": 1,
          "description": "Maximum number of services to return, from 1 to 1000 (default 100).",
          "maximum": 1000
        }
      }
    }
  },
  {
    "name": "svc.logs",
    "description": "Read recent log lines for a background service (journald on Linux, declared plist log files on macOS, or Windows Event Log). When `scope` is omitted, checks \"user\" first and then \"system\" (`conflict` if in both). Read-only and never escalates privileges; long lines are cut and marked.",
    "effect": "read",
    "input_schema": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "name": {
          "type": "string",
          "description": "Service name to read logs for."
        },
        "scope": {
          "description": "`\"user\"` or `\"system\"`. When omitted, checks `\"user\"` first and then `\"system\"`.\n`user`: Per-user service manager (`systemctl --user` on Linux, `gui/<uid>` on macOS).\n`system`: Machine-wide service manager (the only scope on Windows).",
          "type": "string",
          "enum": [
            "user",
            "system"
          ]
        },
        "lines": {
          "type": "integer",
          "default": 100,
          "minimum": 1,
          "description": "Maximum number of lines to return, from 1 to 2000 (default 100).",
          "maximum": 2000
        },
        "since": {
          "type": "string",
          "description": "Start of the log window: an RFC 3339 UTC timestamp (such as `2026-11-23T22:00:00Z`) or a\npositive duration in seconds (such as `\"300\"` or `300`)."
        }
      },
      "required": [
        "name"
      ]
    }
  },
  {
    "name": "svc.restart",
    "description": "Restart a background service and read its state back to confirm it is running. When `scope` is omitted, checks \"user\" first and then \"system\" (`conflict` if in both). Changing a system service requires privileges: on Unix, legion never escalates unless `[svc] sudo = true` is set in node.toml (running `sudo -n`); on Windows the node must run elevated.",
    "effect": "exec",
    "input_schema": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "name": {
          "type": "string",
          "description": "Service name to control."
        },
        "scope": {
          "description": "`\"user\"` or `\"system\"`. When omitted, checks `\"user\"` first and then `\"system\"`.\n`user`: Per-user service manager (`systemctl --user` on Linux, `gui/<uid>` on macOS).\n`system`: Machine-wide service manager (the only scope on Windows).",
          "type": "string",
          "enum": [
            "user",
            "system"
          ]
        }
      },
      "required": [
        "name"
      ]
    }
  },
  {
    "name": "svc.start",
    "description": "Start a background service and read its state back to confirm it is running. When `scope` is omitted, checks \"user\" first and then \"system\" (`conflict` if in both). Changing a system service requires privileges: on Unix, legion never escalates unless `[svc] sudo = true` is set in node.toml (running `sudo -n`); on Windows the node must run elevated.",
    "effect": "exec",
    "input_schema": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "name": {
          "type": "string",
          "description": "Service name to control."
        },
        "scope": {
          "description": "`\"user\"` or `\"system\"`. When omitted, checks `\"user\"` first and then `\"system\"`.\n`user`: Per-user service manager (`systemctl --user` on Linux, `gui/<uid>` on macOS).\n`system`: Machine-wide service manager (the only scope on Windows).",
          "type": "string",
          "enum": [
            "user",
            "system"
          ]
        }
      },
      "required": [
        "name"
      ]
    }
  },
  {
    "name": "svc.status",
    "description": "Read the current state, enabled flag, PID, start time, exit code and diagnostic properties of one background service. When `scope` is omitted, checks \"user\" first and then \"system\", returning which matched or `conflict` if the name exists in both. Read-only and never escalates privileges.",
    "effect": "read",
    "input_schema": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "name": {
          "type": "string",
          "description": "Service name (for example `nginx.service`, `com.example.worker` or `Spooler`)."
        },
        "scope": {
          "description": "`\"user\"` or `\"system\"`. When omitted, checks `\"user\"` first and then `\"system\"`.\n`user`: Per-user service manager (`systemctl --user` on Linux, `gui/<uid>` on macOS).\n`system`: Machine-wide service manager (the only scope on Windows).",
          "type": "string",
          "enum": [
            "user",
            "system"
          ]
        }
      },
      "required": [
        "name"
      ]
    }
  },
  {
    "name": "svc.stop",
    "description": "Stop a background service and read its state back to confirm it is stopped. When `scope` is omitted, checks \"user\" first and then \"system\" (`conflict` if in both). Changing a system service requires privileges: on Unix, legion never escalates unless `[svc] sudo = true` is set in node.toml (running `sudo -n`); on Windows the node must run elevated.",
    "effect": "exec",
    "input_schema": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "name": {
          "type": "string",
          "description": "Service name to control."
        },
        "scope": {
          "description": "`\"user\"` or `\"system\"`. When omitted, checks `\"user\"` first and then `\"system\"`.\n`user`: Per-user service manager (`systemctl --user` on Linux, `gui/<uid>` on macOS).\n`system`: Machine-wide service manager (the only scope on Windows).",
          "type": "string",
          "enum": [
            "user",
            "system"
          ]
        }
      },
      "required": [
        "name"
      ]
    }
  },
  {
    "name": "sys.info",
    "description": "Describe this machine: host name, operating system, uptime and boot id, CPU model, core count and current usage, memory and swap, disks, and NVIDIA GPUs. Sizes are bytes.",
    "effect": "read",
    "input_schema": {
      "type": "object",
      "additionalProperties": false
    }
  },
  {
    "name": "sys.kill",
    "description": "Stop a process: ask it to exit, and force it after a short grace if it has not. Pass the `start_time` sys.procs reported so a process that has since exited and whose id was reused is never hit. `tree` stops its descendants too; `force` skips asking.",
    "effect": "destroy",
    "input_schema": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "pid": {
          "type": "integer",
          "description": "The process id.",
          "minimum": 0
        },
        "start_time": {
          "type": "string",
          "description": "When the process started, as sys.procs reported it. When given and the process with this\nid started at another time, the id has been reused and nothing is stopped."
        },
        "tree": {
          "type": "boolean",
          "description": "Stop the process's descendants as well.",
          "default": false
        },
        "force": {
          "type": "boolean",
          "description": "Stop it at once instead of asking it to exit first.",
          "default": false
        }
      },
      "required": [
        "pid"
      ]
    }
  },
  {
    "name": "sys.procs",
    "description": "List the processes running on this machine, busiest first, with their id, parent, name, command line, user, CPU usage (percent of one core), memory in bytes and start time. `filter` keeps those whose name or command line contains it. Command lines are shown with secret-looking values (passwords, tokens, credentials in URLs) replaced by [redacted].",
    "effect": "read",
    "input_schema": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "filter": {
          "type": "string",
          "description": "Keep only processes whose name or command line contains this, ignoring case."
        },
        "sort": {
          "description": "What the busiest are measured by: `cpu` (the default) or `memory`.\n`cpu`: CPU usage, highest first.\n`memory`: Memory, largest first.",
          "type": "string",
          "enum": [
            "cpu",
            "memory"
          ]
        },
        "limit": {
          "type": "integer",
          "description": "How many processes to return, 1 to 500; 30 when absent.",
          "minimum": 0
        }
      }
    }
  },
  {
    "name": "token.add",
    "description": "Make a bearer token for `legion serve`, standing for one principal that policy.toml names and the caller could act as itself: its own, or any when the caller has the owner role. The token is in this reply and nowhere else: only its hash is kept, so copy it now. A token for a principal with the owner role or any admin (owner-strength) role is the keys to the machine (whoever holds it may change who may do what and do anything the account can, from anywhere the server is published), so only a caller with the owner role may make one, and only with allow_owner true. A token made by a call that came with an expiring token expires no later.",
    "effect": "admin",
    "input_schema": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "name": {
          "type": "string",
          "description": "What to call the token, such as phone: lower-case letters, digits, - and _."
        },
        "principal": {
          "type": "string",
          "description": "The principal every request with the token acts as. policy.toml must name it, which is\nwhat gives it a role."
        },
        "expires_days": {
          "type": "integer",
          "description": "Days until the token stops working; it never does when left out.",
          "minimum": 0
        },
        "allow_owner": {
          "type": "boolean",
          "description": "Allow a token for `owner` or any admin (owner-strength) role, which can change who may do\nwhat. Off unless given as true.",
          "default": false
        }
      },
      "required": [
        "name",
        "principal"
      ]
    }
  },
  {
    "name": "token.list",
    "description": "List the tokens `legion serve` accepts for principals the caller could act as: each one's name, principal, when it was made and when it expires. The tokens themselves are not kept, so they cannot be shown.",
    "effect": "admin",
    "input_schema": {
      "type": "object",
      "additionalProperties": false
    }
  },
  {
    "name": "token.remove",
    "description": "Remove a token of a principal the caller could act as, so `legion serve` refuses it from its next request on and ends its MCP sessions.",
    "effect": "admin",
    "input_schema": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "name": {
          "type": "string",
          "description": "The token's name, as token.list shows it."
        }
      },
      "required": [
        "name"
      ]
    }
  }
];

export const OLD_FIXTURE_TOOL_MAP = new Map(OLD_FIXTURE_TOOLS.map((t) => [t.name, t]));

/**
 * Strictly validates call arguments against a tool's JSON schema, throwing the exact
 * `invalid_args` error message and CLI hint that a real Legion node returns.
 */
export function validateToolArgsAgainstSpec(machine, spec, args) {
  if (!spec || typeof spec !== 'object') return;
  const props = spec.input_schema?.properties || {};
  const allowedKeys = Object.keys(props);
  const input = args && typeof args === 'object' ? args : {};
  for (const key of Object.keys(input)) {
    if (input[key] === undefined) continue;
    if (!Object.prototype.hasOwnProperty.call(props, key)) {
      const expected =
        allowedKeys.length > 0
          ? `expected one of ${allowedKeys.map((k) => `\`${k}\``).join(', ')}`
          : 'expected no arguments';
      throw {
        code: 'invalid_args',
        message: `${spec.name}: \`${key}\`: unknown field \`${key}\`, ${expected}.`,
        hint: `\`legion help ${spec.name}\` shows the arguments it takes.`,
        machine: machine || null,
      };
    }
  }
  const required = Array.isArray(spec.input_schema?.required) ? spec.input_schema.required : [];
  for (const reqKey of required) {
    if (input[reqKey] === undefined || input[reqKey] === null) {
      throw {
        code: 'invalid_args',
        message: `${spec.name}: missing field \`${reqKey}\`.`,
        hint: `\`legion help ${spec.name}\` shows the arguments it takes.`,
        machine: machine || null,
      };
    }
  }
}
