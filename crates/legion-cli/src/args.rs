//! The command line, read into a [`Command`] before anything runs, so a mistake in it is always a
//! usage error and never a half-done call.

use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;

use legion_core::policy::Principal;
use legion_core::proto::Target;
use serde_json::{Map, Value};

/// What the program was asked to do.
#[derive(Debug, PartialEq)]
pub enum Command {
    /// `legion <tool> …`: call one tool.
    Call(CallCommand),
    /// `legion tools`: list the tools.
    Tools {
        /// Print JSON even on a terminal.
        json: bool,
    },
    /// `legion help [<tool>]`: describe one tool, or the program.
    Help {
        /// The tool, if one was named.
        tool: Option<String>,
        /// Print JSON even on a terminal.
        json: bool,
    },
    /// `legion rpc`: serve JSON-RPC on stdin and stdout.
    Rpc {
        /// The principal every call in the session acts as.
        pinned: Option<String>,
    },
    /// `legion mcp`: serve the tools to an AI agent over MCP on stdin and stdout.
    Mcp {
        /// The principal every call in the session acts as.
        pinned: Option<String>,
        /// Serve four tools that list, describe and run the others.
        compact: bool,
    },
    /// `legion stream`: serve one bulk-data operation on stdin and stdout.
    Stream {
        /// The principal the operation acts as.
        pinned: Option<String>,
    },
    /// `legion gate --as P` or `legion gate --enrol ID`: the forced command of a restricted ssh key.
    Gate(GateCommand),
    /// `legion pair --as P ...`: create a pairing invite and show it as a QR code and text.
    Pair(PairCommand),
    /// `legion cp <from> <to> …`: `fs.transfer` from a command line.
    Cp(CpCommand),
    /// `legion serve`: serve the tools over HTTP on a loopback address.
    Serve {
        /// `--listen`.
        listen: Option<String>,
        /// `--help`: say how, rather than serve.
        help: bool,
    },
    /// `legion --version`.
    Version,
}

/// Pinned Legion home passed to `legion gate`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GateHome {
    /// No explicit home flags on a hand-written gate line (ignores `LEGION_HOME` and `XDG_*`).
    Default,
    /// `--home <root>` (`<root>/config` and `<root>/state`).
    Root(PathBuf),
    /// `--config-dir <config> --state-dir <state>`.
    Split {
        /// Pinned configuration directory (`node.toml`, `policy.toml`, ...).
        config_dir: PathBuf,
        /// Pinned state directory (`audit.jsonl`, `devices.json`, ...).
        state_dir: PathBuf,
    },
}

/// `legion gate --as P [--device NAME] [--account USER] [--home PATH | --config-dir CFG --state-dir ST]`
/// or `legion gate --enrol ID [--account USER] [--home PATH | --config-dir CFG --state-dir ST]`.
#[derive(Debug, PartialEq)]
pub enum GateCommand {
    /// Confine an ssh key to `legion rpc` and `legion stream` as `pinned`.
    As {
        /// The principal everything the key does acts as.
        pinned: String,
        /// Optional paired device name for live-session tracking and immediate revocation.
        device: Option<String>,
        /// Optional pinned OS account name (used on shared `administrators_authorized_keys`).
        account: Option<String>,
        /// Pinned Legion home directories (`--home` or `--config-dir`/`--state-dir`).
        home: GateHome,
    },
    /// Single-use forced command that enrols one device for invite `id`.
    Enrol {
        /// The invite identifier issued by `device.invite`.
        id: String,
        /// Optional pinned OS account name (used on shared `administrators_authorized_keys`).
        account: Option<String>,
        /// Pinned Legion home directories (`--home` or `--config-dir`/`--state-dir`).
        home: GateHome,
    },
}

/// `legion pair --as P [--valid 15m] [--host H]... [--port N] [--user U] [--allow-owner] [--invert] [--on M] [--json]`.
#[derive(Debug, PartialEq)]
pub struct PairCommand {
    /// `--as`: the principal the paired device will act as.
    pub principal: String,
    /// `--valid`: validity in minutes (`1..=1440`).
    pub valid_minutes: Option<u32>,
    /// `--host`: optional explicit hosts overriding automatic detection.
    pub hosts: Vec<String>,
    /// `--port`: optional SSH port.
    pub port: Option<u16>,
    /// `--user`: optional OS account name.
    pub user: Option<String>,
    /// `--allow-owner`: explicitly permit pairing a principal with the `owner` role.
    pub allow_owner: bool,
    /// `--invert`: invert the QR code dark/light polarity for light terminals.
    pub invert: bool,
    /// `--on`: optional remote machine to issue the invite on.
    pub on: Option<String>,
    /// `--json`: print the raw `device.invite` JSON result.
    pub json: bool,
}

/// `legion cp <from> <to> [--delete] [--checksum] [--exclude G]... [--dry-run] [--as P] [--json]`.
#[derive(Debug, PartialEq)]
pub struct CpCommand {
    /// What to send.
    pub from: String,
    /// Where it goes.
    pub to: String,
    /// `--delete`.
    pub delete: bool,
    /// `--checksum`.
    pub checksum: bool,
    /// Every `--exclude`.
    pub exclude: Vec<String>,
    /// `--dry-run`.
    pub dry_run: bool,
    /// `--as`.
    pub as_principal: Option<String>,
    /// `--json`.
    pub json: bool,
}

/// `legion <tool> [key=value …] [--args JSON|-] [--as P] [--on M] [--json]`.
#[derive(Debug, PartialEq)]
pub struct CallCommand {
    /// The tool's name.
    pub tool: String,
    /// Where its arguments come from.
    pub args: ArgsSource,
    /// `--as`.
    pub as_principal: Option<String>,
    /// `--on`.
    pub on: Option<Target>,
    /// `--json`.
    pub json: bool,
}

/// A call's arguments: `key=value` pairs, or one JSON object given whole.
#[derive(Debug, PartialEq)]
pub enum ArgsSource {
    /// From `key=value` pairs; each value is JSON when it parses as JSON and a string otherwise.
    Pairs(Map<String, Value>),
    /// From `--args '<json object>'`.
    Json(String),
    /// From `--args -`: the JSON object on stdin.
    Stdin,
}

/// A command line that cannot be run as written.
#[derive(Debug, PartialEq)]
pub struct UsageError(pub String);

impl fmt::Display for UsageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The usage summary printed by `legion help` and after a usage error.
pub const USAGE: &str = "\
Usage:
  legion <tool> [key=value ...] [--args JSON | --args -] [--as PRINCIPAL] [--on MACHINE] [--json]
  legion tools [--json]
  legion help [<tool>] [--json]
  legion cp <from> <to> [--delete] [--checksum] [--exclude GLOB]... [--dry-run] [--as PRINCIPAL] [--json]
  legion rpc [--as PRINCIPAL]
  legion mcp [--as PRINCIPAL] [--compact]
  legion stream [--as PRINCIPAL]
  legion gate --as PRINCIPAL | --enrol ID
  legion serve [--listen ADDRESS]
  legion init [--name NAME] [--no-roles] [--dry-run] [--on MACHINE] [--json]
  legion pair --as PRINCIPAL [--valid 15m] [--host HOST]... [--port PORT] [--user USER] [--invert] [--json]
  legion agents list | add AGENT --as PRINCIPAL [--full] [--scope SCOPE] [--allow-owner] [--dry-run] | remove AGENT [--scope SCOPE] [--dry-run]
  legion token add NAME PRINCIPAL [expires_days=N] [allow_owner=true] | list | remove NAME
  legion --version

A value that parses as JSON is JSON (n=3, ok=true, tags=[\"a\"]); anything else is a string.
--args takes the arguments as one JSON object instead, or reads it from stdin when given -.
Output is readable on a terminal and JSON otherwise; --json asks for JSON anywhere.
`legion help cp`, `legion help init`, `legion help pair`, `legion help agents`, `legion help stream` and `legion help gate` describe those commands.";

/// `legion help init`.
pub const INIT_HELP: &str = "\
Usage: legion init [--name NAME] [--no-roles] [--dry-run] [--as PRINCIPAL] [--on MACHINE] [--json]

Runs meta.init to create starter node.toml and policy.toml files where they do not already exist.
Existing files are never overwritten or reformatted, so it is safe to run again.

  --name NAME   node name to write into node.toml when creating it
  --no-roles    skip creating policy.toml with starter roles
  --dry-run     report what would be created or kept without writing anything
  --as P        act as the principal P
  --on M        initialise machine M instead of this node
  --json        print the result as JSON";

/// `legion help agents`.
pub const AGENTS_HELP: &str = "\
Usage:
  legion agents list [--as PRINCIPAL] [--on MACHINE] [--json]
  legion agents add <claude|codex|antigravity|opencode> --as PRINCIPAL [--compact | --full] [--scope SCOPE] [--allow-owner] [--dry-run] [--on MACHINE] [--json]
  legion agents remove <claude|codex|antigravity|opencode> [--scope SCOPE] [--dry-run] [--as PRINCIPAL] [--on MACHINE] [--json]

Registers, lists, or removes the `legion` MCP server in supported AI coding agents through each
agent's own CLI command, never by editing their configuration files directly.

In `legion agents add`, `--as PRINCIPAL` names the principal the agent will act as (and must exist
in policy.toml, or be `owner` with `--allow-owner`). Compact mode is enabled by default so the
agent carries four meta-tools instead of every tool schema in every conversation; pass `--full`
(or `--no-compact`) to expose every tool individually.";

/// `legion help cp`.
pub const CP_HELP: &str = "\
Usage: legion cp <from> <to> [--delete] [--checksum] [--exclude GLOB]... [--dry-run] [--as PRINCIPAL] [--json]

Copies a file or a directory tree with fs.transfer. Each of <from> and <to> is machine:/path, for a
machine in fleet.toml, or a path on this machine; ~ means the home directory of the machine the
path is on. <to> names the destination itself, except that a file sent to an existing directory, or
to a path ending in /, lands inside it under its own name. A Windows drive path (C:\\... or C:/...)
is always a path on this machine.

A file that was interrupted continues where it stopped. A tree sends only what differs, so running
the same command again sends nothing.

  --delete      remove from the destination tree what the source no longer has
  --checksum    compare files by hash instead of size and modification time
  --exclude G   leave out paths matching the glob G at both ends (repeatable; a trailing / matches
                directories only)
  --dry-run     say what would be sent and deleted, and change nothing
  --as P        act as the principal P
  --json        print the result as JSON

Progress is shown on stderr when it is a terminal.";

/// `legion help stream`.
pub const STREAM_HELP: &str = "\
Usage: legion stream [--as PRINCIPAL]

The pipe file transfers move bytes over; fs.transfer and legion cp start it on the other machine.
It reads one JSON header on stdin, saying what to do (manifest, stat, get, put, pack, unpack or
delete) and where, then moves raw bytes. Every path is inside that header, so the command line is
always exactly `legion stream`. Each operation is allowed as the file tool it amounts to would be
(fs.read, fs.write or fs.remove) and is written to the audit log with the door stream.";

/// `legion help pair`.
pub const PAIR_HELP: &str = "\
Usage: legion pair --as PRINCIPAL [--valid 15m] [--host HOST]... [--port PORT] [--user USER] [--allow-owner] [--invert] [--on MACHINE] [--json]

Creates a single-use pairing invite for a phone, tablet or other controller by running
device.invite, then draws the invite as a QR code and prints the invite text on one line.

The invite carries where to reach this machine over SSH, its pinned sshd host keys, and a
single-use temporary key that expires automatically. When the device scans or pastes the invite,
it connects once to hand over its own public key and receives a permanent key confined by
`legion gate --as PRINCIPAL`.

  --as P          principal from policy.toml that the paired device will act as (required)
  --valid DUR     how long the invite stays valid (e.g. 15m, 1h, 24h; default 15m, max 24h)
  --host H        host name or address to put in the invite (repeatable; overrides detection)
  --port N        SSH port on this machine (default 22)
  --user U        OS account name on this machine (defaults to the current user)
  --allow-owner   permit pairing a principal with the unrestricted owner role
  --invert        swap QR code foreground and background (useful on light terminals)
  --on M          create the invite on machine M instead of this node
  --json          print the device.invite tool result as JSON

Manage paired devices and pending invites with `legion device.list` and
`legion device.remove name=<device>` (or `id=<invite>`).";

/// `legion help gate`.
pub const GATE_HELP: &str = "\
Usage:
  legion gate --as PRINCIPAL
  legion gate --enrol ID

The forced command of an ssh key that may do exactly what PRINCIPAL's role in policy.toml allows on
this machine, and nothing else. sshd runs it whatever the client asked for. With `--as PRINCIPAL`,
it reads the request from SSH_ORIGINAL_COMMAND without a shell and, when that is exactly one of the
commands legion's ssh connector sends for `legion rpc` or `legion stream`, serves it itself, pinned
to PRINCIPAL. Anything else is refused with exit code 1.

With `--enrol ID`, it serves the single-use device enrolment handshake for the temporary invite key
added by `device.invite` (or `legion pair`), replacing the temporary line with a permanent
`legion gate --as PRINCIPAL` line and ignoring SSH_ORIGINAL_COMMAND.

In ~/.ssh/authorized_keys on this machine, one line per key:

  command=\"/home/me/.local/bin/legion gate --as agent:example\",restrict ssh-ed25519 AAAA... comment

Use the full path to legion; restrict turns off port forwarding, a terminal and the rest.";

/// Reads the arguments after the program's name.
pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Command, UsageError> {
    let args: Vec<String> = args
        .into_iter()
        .map(|arg| {
            arg.into_string().map_err(|arg| {
                UsageError(format!(
                    "{} is not valid Unicode, and arguments are JSON text.",
                    arg.to_string_lossy()
                ))
            })
        })
        .collect::<Result<_, _>>()?;
    let Some((first, rest)) = args.split_first() else {
        return Err(UsageError("Name a tool to call, or a command.".to_owned()));
    };
    match first.as_str() {
        "--version" | "-V" => no_more(rest).map(|()| Command::Version),
        "--help" | "-h" => no_more(rest).map(|()| Command::Help {
            tool: None,
            json: false,
        }),
        "tools" => {
            let mut flags = Flags::read(rest, &["--json"])?;
            flags.no_pairs("tools")?;
            Ok(Command::Tools { json: flags.json })
        }
        "help" => {
            let mut flags = Flags::read(rest, &["--json"])?;
            let tool = flags.positional.pop();
            if !flags.positional.is_empty() || !flags.pairs.is_empty() {
                return Err(UsageError("help takes one tool's name.".to_owned()));
            }
            Ok(Command::Help {
                tool,
                json: flags.json,
            })
        }
        "rpc" => {
            let mut flags = Flags::read(rest, &["--as"])?;
            flags.no_pairs("rpc")?;
            Ok(Command::Rpc {
                pinned: flags.as_principal,
            })
        }
        "mcp" => {
            let mut flags = Flags::read(rest, &["--as", "--compact"])?;
            flags.no_pairs("mcp")?;
            Ok(Command::Mcp {
                pinned: flags.as_principal,
                compact: flags.compact,
            })
        }
        "stream" => {
            let mut flags = Flags::read(rest, &["--as"])?;
            flags.no_pairs("stream")?;
            Ok(Command::Stream {
                pinned: flags.as_principal,
            })
        }
        "gate" => gate(rest),
        "pair" => pair(rest),
        "cp" => cp(rest).map(Command::Cp),
        "serve" => serve(rest),
        "init" => init(rest),
        "agents" => agents(rest),
        "token" => token(rest),
        tool if tool.starts_with('-') => Err(UsageError(format!(
            "{tool} is not an option here; options come after the tool's name."
        ))),
        tool if !tool.contains('.') => Err(UsageError(format!(
            "{tool} is not a command, and tool names have a dot, such as meta.version. `legion tools` lists them."
        ))),
        tool => {
            let flags = Flags::read(rest, &["--json", "--as", "--on", "--args"])?;
            if let Some(stray) = flags.positional.first() {
                return Err(UsageError(format!(
                    "{stray} is not key=value. Write each argument as key=value, or pass them all with --args."
                )));
            }
            let args = match flags.args {
                Some(_) if !flags.pairs.is_empty() => {
                    return Err(UsageError(
                        "Give arguments as key=value or with --args, not both.".to_owned(),
                    ));
                }
                Some(json) if json == "-" => ArgsSource::Stdin,
                Some(json) => ArgsSource::Json(json),
                None => ArgsSource::Pairs(flags.pairs),
            };
            Ok(Command::Call(CallCommand {
                tool: tool.to_owned(),
                args,
                as_principal: flags.as_principal,
                on: flags.on,
                json: flags.json,
            }))
        }
    }
}

/// Reads `legion cp`'s words: two places, and options anywhere among them.
fn cp(rest: &[String]) -> Result<CpCommand, UsageError> {
    let mut places = Vec::new();
    let mut command = CpCommand {
        from: String::new(),
        to: String::new(),
        delete: false,
        checksum: false,
        exclude: Vec::new(),
        dry_run: false,
        as_principal: None,
        json: false,
    };
    let mut words = rest.iter();
    let mut options_ended = false;
    while let Some(word) = words.next() {
        if options_ended || !word.starts_with("--") {
            places.push(word.clone());
            continue;
        }
        let (name, inline) = match word.split_once('=') {
            Some((name, value)) => (name, Some(value.to_owned())),
            None => (word.as_str(), None),
        };
        let mut value = |name: &str| {
            inline
                .clone()
                .or_else(|| words.next().cloned())
                .ok_or_else(|| UsageError(format!("{name} needs a value.")))
        };
        let flag = |set: &mut bool| {
            if inline.is_some() {
                return Err(UsageError(format!("{name} takes no value.")));
            }
            *set = true;
            Ok(())
        };
        match name {
            "--" if inline.is_none() => options_ended = true,
            "--delete" => flag(&mut command.delete)?,
            "--checksum" => flag(&mut command.checksum)?,
            "--dry-run" => flag(&mut command.dry_run)?,
            "--json" => flag(&mut command.json)?,
            "--exclude" => command.exclude.push(value(name)?),
            "--as" => {
                let principal = value(name)?;
                Principal::parse(&principal)
                    .map_err(|error| UsageError(format!("--as: {}", error.message)))?;
                if command.as_principal.replace(principal).is_some() {
                    return Err(UsageError("--as is given twice.".to_owned()));
                }
            }
            _ => return Err(UsageError(format!("{name} is not an option of cp."))),
        }
    }
    let [from, to] = <[String; 2]>::try_from(places).map_err(|places| {
        UsageError(format!(
            "cp takes two places, what to send and where, and {} were given.",
            places.len()
        ))
    })?;
    command.from = from;
    command.to = to;
    Ok(command)
}

/// The help of a command that is not a tool, for `legion help <command>`.
pub fn command_help(name: &str) -> Option<&'static str> {
    match name {
        "cp" => Some(CP_HELP),
        "init" => Some(INIT_HELP),
        "pair" => Some(PAIR_HELP),
        "agents" => Some(AGENTS_HELP),
        "stream" => Some(STREAM_HELP),
        "gate" => Some(GATE_HELP),
        _ => None,
    }
}

/// `legion gate --as PRINCIPAL [--device NAME] [--account USER] [--home PATH | --config-dir CFG --state-dir ST]`
/// or `--enrol ID [--account USER] [--home PATH | --config-dir CFG --state-dir ST]`.
fn gate(rest: &[String]) -> Result<Command, UsageError> {
    let mut pinned = None;
    let mut device = None;
    let mut account = None;
    let mut enrol_id = None;
    let mut home: Option<PathBuf> = None;
    let mut config_dir: Option<PathBuf> = None;
    let mut state_dir: Option<PathBuf> = None;
    let mut words = rest.iter();
    while let Some(word) = words.next() {
        if !word.starts_with('-') {
            return Err(UsageError(format!(
                "gate takes no positional arguments, and {word} was given."
            )));
        }
        let (name, inline) = match word.split_once('=') {
            Some((name, value)) => (name, Some(value.to_owned())),
            None => (word.as_str(), None),
        };
        let mut take_value = |opt: &str| {
            inline
                .clone()
                .or_else(|| words.next().cloned())
                .ok_or_else(|| UsageError(format!("{opt} needs a value.")))
        };
        match name {
            "--as" => {
                let value = take_value(name)?;
                Principal::parse(&value)
                    .map_err(|error| UsageError(format!("--as: {}", error.message)))?;
                if pinned.replace(value).is_some() {
                    return Err(UsageError("--as is given twice.".to_owned()));
                }
            }
            "--device" => {
                let value = take_value(name)?;
                legion_tools::device::sessions::validate_device_label(&value)
                    .map_err(|error| UsageError(format!("--device: {}", error.message)))?;
                if device.replace(value).is_some() {
                    return Err(UsageError("--device is given twice.".to_owned()));
                }
            }
            "--account" => {
                let value = take_value(name)?;
                legion_tools::device::authkeys::validate_account_name(&value)
                    .map_err(|error| UsageError(format!("--account: {}", error.message)))?;
                if account.replace(value).is_some() {
                    return Err(UsageError("--account is given twice.".to_owned()));
                }
            }
            "--enrol" => {
                let value = take_value(name)?;
                legion_tools::device::keys::validate_invite_id(&value)
                    .map_err(|error| UsageError(format!("--enrol: {}", error.message)))?;
                if enrol_id.replace(value).is_some() {
                    return Err(UsageError("--enrol is given twice.".to_owned()));
                }
            }
            "--home" => {
                let value = take_value(name)?;
                let path = PathBuf::from(&value);
                if value.is_empty() || !path.is_absolute() {
                    return Err(UsageError(format!(
                        "--home must be an absolute path, not {value:?}."
                    )));
                }
                if home.replace(path).is_some() {
                    return Err(UsageError("--home is given twice.".to_owned()));
                }
            }
            "--config-dir" => {
                let value = take_value(name)?;
                let path = PathBuf::from(&value);
                if value.is_empty() || !path.is_absolute() {
                    return Err(UsageError(format!(
                        "--config-dir must be an absolute path, not {value:?}."
                    )));
                }
                if config_dir.replace(path).is_some() {
                    return Err(UsageError("--config-dir is given twice.".to_owned()));
                }
            }
            "--state-dir" => {
                let value = take_value(name)?;
                let path = PathBuf::from(&value);
                if value.is_empty() || !path.is_absolute() {
                    return Err(UsageError(format!(
                        "--state-dir must be an absolute path, not {value:?}."
                    )));
                }
                if state_dir.replace(path).is_some() {
                    return Err(UsageError("--state-dir is given twice.".to_owned()));
                }
            }
            _ => return Err(UsageError(format!("{name} is not an option of gate."))),
        }
    }
    let gate_home = match (home, config_dir, state_dir) {
        (Some(root), None, None) => GateHome::Root(root),
        (None, Some(config_dir), Some(state_dir)) => GateHome::Split {
            config_dir,
            state_dir,
        },
        (None, None, None) => GateHome::Default,
        (Some(_), Some(_), _) | (Some(_), _, Some(_)) => {
            return Err(UsageError(
                "gate takes --home or --config-dir/--state-dir, not both.".to_owned(),
            ));
        }
        (None, Some(_), None) | (None, None, Some(_)) => {
            return Err(UsageError(
                "gate requires --config-dir and --state-dir to be given together.".to_owned(),
            ));
        }
    };
    match (pinned, enrol_id) {
        (Some(pinned), None) => Ok(Command::Gate(GateCommand::As {
            pinned,
            device,
            account,
            home: gate_home,
        })),
        (None, Some(id)) => {
            if device.is_some() {
                return Err(UsageError(
                    "--device is only valid with `gate --as`, not `gate --enrol`.".to_owned(),
                ));
            }
            Ok(Command::Gate(GateCommand::Enrol {
                id,
                account,
                home: gate_home,
            }))
        }
        (Some(_), Some(_)) => Err(UsageError(
            "gate takes --as or --enrol, not both.".to_owned(),
        )),
        (None, None) => Err(UsageError(
            "gate needs --as or --enrol. `legion help gate` shows the authorized_keys line."
                .to_owned(),
        )),
    }
}

/// `legion pair --as PRINCIPAL [--valid 15m] [--host H]... [--port N] [--user U] [--allow-owner] [--invert] [--on M] [--json]`.
fn pair(rest: &[String]) -> Result<Command, UsageError> {
    let mut principal = None;
    let mut valid_minutes = None;
    let mut hosts = Vec::new();
    let mut port = None;
    let mut user = None;
    let mut allow_owner = false;
    let mut invert = false;
    let mut on = None;
    let mut json = false;
    let mut words = rest.iter();
    while let Some(word) = words.next() {
        if !word.starts_with('-') {
            return Err(UsageError(format!(
                "{word} was not expected here. Use --as PRINCIPAL and optional flags; see `legion help pair`."
            )));
        }
        let (name, inline) = match word.split_once('=') {
            Some((name, value)) => (name, Some(value.to_owned())),
            None => (word.as_str(), None),
        };
        let mut take_value = |opt: &str| {
            inline
                .clone()
                .or_else(|| words.next().cloned())
                .ok_or_else(|| UsageError(format!("{opt} needs a value.")))
        };
        let no_inline = |opt: &str| {
            if inline.is_some() {
                Err(UsageError(format!("{opt} takes no value.")))
            } else {
                Ok(())
            }
        };
        match name {
            "--as" => {
                let value = take_value(name)?;
                Principal::parse(&value)
                    .map_err(|error| UsageError(format!("--as: {}", error.message)))?;
                if principal.replace(value).is_some() {
                    return Err(UsageError("--as is given twice.".to_owned()));
                }
            }
            "--valid" => {
                let value = take_value(name)?;
                let minutes = parse_valid_minutes(&value)?;
                if valid_minutes.replace(minutes).is_some() {
                    return Err(UsageError("--valid is given twice.".to_owned()));
                }
            }
            "--host" => {
                let value = take_value(name)?;
                if value.trim().is_empty() {
                    return Err(UsageError("--host cannot be empty.".to_owned()));
                }
                hosts.push(value);
            }
            "--port" => {
                let value = take_value(name)?;
                let parsed: u16 =
                    value.parse().ok().filter(|&p| p >= 1).ok_or_else(|| {
                        UsageError(format!("--port must be 1..65535, not {value}."))
                    })?;
                if port.replace(parsed).is_some() {
                    return Err(UsageError("--port is given twice.".to_owned()));
                }
            }
            "--user" => {
                let value = take_value(name)?;
                if value.trim().is_empty() {
                    return Err(UsageError("--user cannot be empty.".to_owned()));
                }
                if user.replace(value).is_some() {
                    return Err(UsageError("--user is given twice.".to_owned()));
                }
            }
            "--allow-owner" => {
                no_inline(name)?;
                allow_owner = true;
            }
            "--invert" => {
                no_inline(name)?;
                invert = true;
            }
            "--on" => {
                let value = take_value(name)?;
                let target =
                    Target::parse(&value).map_err(|error| UsageError(format!("--on: {error}.")))?;
                let Target::One(machine) = target else {
                    return Err(UsageError(
                        "--on for pair must name a single machine.".to_owned(),
                    ));
                };
                if on.replace(machine).is_some() {
                    return Err(UsageError("--on is given twice.".to_owned()));
                }
            }
            "--json" => {
                no_inline(name)?;
                json = true;
            }
            _ => return Err(UsageError(format!("{name} is not an option of pair."))),
        }
    }
    let principal = principal.ok_or_else(|| {
        UsageError(
            "pair needs --as: the principal from policy.toml the paired device will act as."
                .to_owned(),
        )
    })?;
    Ok(Command::Pair(PairCommand {
        principal,
        valid_minutes,
        hosts,
        port,
        user,
        allow_owner,
        invert,
        on,
        json,
    }))
}

/// Parses `--valid` into whole minutes (`1..=1440`). Accepts plain minutes (`15`), or
/// single/compound unit strings using `d`, `h`, and `m` (`15m`, `1h`, `1h30m`, `24h`, `1d`).
fn parse_valid_minutes(text: &str) -> Result<u32, UsageError> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(UsageError(
            "--valid needs a duration such as 15m, 1h or 24h.".to_owned(),
        ));
    }
    let minutes: u32 = if trimmed.bytes().all(|b| b.is_ascii_digit()) {
        trimmed.parse().map_err(|_| {
            UsageError(format!(
                "--valid {trimmed} is too large; maximum is 1440 minutes (24h)."
            ))
        })?
    } else {
        let mut total: u32 = 0;
        let bytes = trimmed.as_bytes();
        let mut i = 0;
        let mut saw_part = false;
        while i < bytes.len() {
            let num_start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            if i == num_start || i >= bytes.len() {
                return Err(UsageError(format!(
                    "--valid {trimmed} is not a valid duration; use whole minutes like 15m, 1h, 1h30m or 24h."
                )));
            }
            let amount: u32 = trimmed[num_start..i].parse().map_err(|_| {
                UsageError(format!(
                    "--valid {trimmed} is too large; maximum is 24h (1440 minutes)."
                ))
            })?;
            let unit = bytes[i].to_ascii_lowercase();
            i += 1;
            let factor: u32 = match unit {
                b'm' => 1,
                b'h' => 60,
                b'd' => 1440,
                _ => {
                    return Err(UsageError(format!(
                        "--valid {trimmed} uses an unsupported unit; use m, h or d (e.g. 15m, 1h, 24h)."
                    )));
                }
            };
            let part = amount.checked_mul(factor).ok_or_else(|| {
                UsageError(format!(
                    "--valid {trimmed} is too large; maximum is 24h (1440 minutes)."
                ))
            })?;
            total = total.checked_add(part).ok_or_else(|| {
                UsageError(format!(
                    "--valid {trimmed} is too large; maximum is 24h (1440 minutes)."
                ))
            })?;
            saw_part = true;
        }
        if !saw_part {
            return Err(UsageError(
                "--valid needs a duration such as 15m, 1h or 24h.".to_owned(),
            ));
        }
        total
    };
    if !(1..=1440).contains(&minutes) {
        return Err(UsageError(format!(
            "--valid must be between 1m and 24h (1440 minutes), not {trimmed}."
        )));
    }
    Ok(minutes)
}

/// `legion serve [--listen ADDRESS]`, or `--help`.
fn serve(rest: &[String]) -> Result<Command, UsageError> {
    let mut listen = None;
    let mut help = false;
    let mut words = rest.iter();
    while let Some(word) = words.next() {
        let value = if word == "--help" || word == "-h" {
            help = true;
            continue;
        } else if let Some(value) = word.strip_prefix("--listen=") {
            value.to_owned()
        } else if word == "--listen" {
            words
                .next()
                .cloned()
                .ok_or_else(|| UsageError("--listen needs a value.".to_owned()))?
        } else {
            return Err(UsageError(format!("{word} was not expected here.")));
        };
        if listen.replace(value).is_some() {
            return Err(UsageError("--listen is given twice.".to_owned()));
        }
    }
    Ok(Command::Serve { listen, help })
}

/// `legion init`, sugar over `meta.init`.
fn init(rest: &[String]) -> Result<Command, UsageError> {
    let mut pairs = Map::new();
    let mut as_principal = None;
    let mut on = None;
    let mut json = false;
    let mut words = rest.iter();
    while let Some(word) = words.next() {
        if word.starts_with('-') {
            let (name, inline) = match word.split_once('=') {
                Some((name, value)) => (name, Some(value.to_owned())),
                None => (word.as_str(), None),
            };
            let mut take_value = |opt: &str| {
                inline
                    .clone()
                    .or_else(|| words.next().cloned())
                    .ok_or_else(|| UsageError(format!("{opt} needs a value.")))
            };
            let no_inline = |opt: &str| {
                if inline.is_some() {
                    Err(UsageError(format!("{opt} takes no value.")))
                } else {
                    Ok(())
                }
            };
            match name {
                "--name" => {
                    let value = take_value(name)?;
                    if pairs
                        .insert("name".to_owned(), Value::String(value))
                        .is_some()
                    {
                        return Err(UsageError("name is given twice.".to_owned()));
                    }
                }
                "--no-roles" => {
                    no_inline(name)?;
                    if pairs
                        .insert("roles".to_owned(), Value::Bool(false))
                        .is_some()
                    {
                        return Err(UsageError("roles is given twice.".to_owned()));
                    }
                }
                "--dry-run" => {
                    no_inline(name)?;
                    if pairs
                        .insert("dry_run".to_owned(), Value::Bool(true))
                        .is_some()
                    {
                        return Err(UsageError("dry_run is given twice.".to_owned()));
                    }
                }
                "--json" => {
                    no_inline(name)?;
                    json = true;
                }
                "--as" => {
                    let value = take_value(name)?;
                    Principal::parse(&value)
                        .map_err(|error| UsageError(format!("--as: {}", error.message)))?;
                    if as_principal.replace(value).is_some() {
                        return Err(UsageError("--as is given twice.".to_owned()));
                    }
                }
                "--on" => {
                    let value = take_value(name)?;
                    let target = Target::parse(&value)
                        .map_err(|error| UsageError(format!("--on: {error}.")))?;
                    if on.replace(target).is_some() {
                        return Err(UsageError("--on is given twice.".to_owned()));
                    }
                }
                _ => return Err(UsageError(format!("{name} is not an option of init."))),
            }
            continue;
        }
        match word.split_once('=') {
            Some((key, value)) if !key.is_empty() => {
                if pairs.contains_key(key) {
                    return Err(UsageError(format!("{key} is given twice.")));
                }
                pairs.insert(key.to_owned(), value_of(value));
            }
            _ => {
                return Err(UsageError(format!(
                    "{word} was not expected here. Use --name NAME, --no-roles, --dry-run, or key=value."
                )));
            }
        }
    }
    Ok(Command::Call(CallCommand {
        tool: "meta.init".to_owned(),
        args: ArgsSource::Pairs(pairs),
        as_principal,
        on,
        json,
    }))
}

/// `legion agents list|add|remove`, sugar over `agent.list`, `agent.add`, and `agent.remove`.
fn agents(rest: &[String]) -> Result<Command, UsageError> {
    let Some((verb, rest)) = rest.split_first() else {
        return Err(UsageError("agents takes list, add or remove.".to_owned()));
    };
    match verb.as_str() {
        "list" => {
            let mut flags = Flags::read(rest, &["--json", "--as", "--on"])?;
            flags.no_pairs("agents list")?;
            Ok(Command::Call(CallCommand {
                tool: "agent.list".to_owned(),
                args: ArgsSource::Pairs(Map::new()),
                as_principal: flags.as_principal,
                on: flags.on,
                json: flags.json,
            }))
        }
        "add" => agents_add(rest),
        "remove" => agents_remove(rest),
        other => Err(UsageError(format!(
            "{other} is not something agents does; it takes list, add or remove."
        ))),
    }
}

fn agents_add(rest: &[String]) -> Result<Command, UsageError> {
    let mut pairs = Map::new();
    let mut positional = Vec::new();
    let mut on = None;
    let mut json = false;
    let mut words = rest.iter();
    while let Some(word) = words.next() {
        if word.starts_with('-') {
            let (name, inline) = match word.split_once('=') {
                Some((name, value)) => (name, Some(value.to_owned())),
                None => (word.as_str(), None),
            };
            let mut take_value = |opt: &str| {
                inline
                    .clone()
                    .or_else(|| words.next().cloned())
                    .ok_or_else(|| UsageError(format!("{opt} needs a value.")))
            };
            let no_inline = |opt: &str| {
                if inline.is_some() {
                    Err(UsageError(format!("{opt} takes no value.")))
                } else {
                    Ok(())
                }
            };
            match name {
                "--as" => {
                    let principal = take_value(name)?;
                    Principal::parse(&principal)
                        .map_err(|error| UsageError(format!("--as: {}", error.message)))?;
                    if pairs
                        .insert("as".to_owned(), Value::String(principal))
                        .is_some()
                    {
                        return Err(UsageError("as is given twice.".to_owned()));
                    }
                }
                "--compact" => {
                    no_inline(name)?;
                    if pairs
                        .insert("compact".to_owned(), Value::Bool(true))
                        .is_some()
                    {
                        return Err(UsageError("compact is given twice.".to_owned()));
                    }
                }
                "--full" | "--no-compact" => {
                    no_inline(name)?;
                    if pairs
                        .insert("compact".to_owned(), Value::Bool(false))
                        .is_some()
                    {
                        return Err(UsageError("compact is given twice.".to_owned()));
                    }
                }
                "--scope" => {
                    let scope = take_value(name)?;
                    if pairs
                        .insert("scope".to_owned(), Value::String(scope))
                        .is_some()
                    {
                        return Err(UsageError("scope is given twice.".to_owned()));
                    }
                }
                "--allow-owner" => {
                    no_inline(name)?;
                    if pairs
                        .insert("allow_owner".to_owned(), Value::Bool(true))
                        .is_some()
                    {
                        return Err(UsageError("allow_owner is given twice.".to_owned()));
                    }
                }
                "--dry-run" => {
                    no_inline(name)?;
                    if pairs
                        .insert("dry_run".to_owned(), Value::Bool(true))
                        .is_some()
                    {
                        return Err(UsageError("dry_run is given twice.".to_owned()));
                    }
                }
                "--json" => {
                    no_inline(name)?;
                    json = true;
                }
                "--on" => {
                    let value = take_value(name)?;
                    let target = Target::parse(&value)
                        .map_err(|error| UsageError(format!("--on: {error}.")))?;
                    if on.replace(target).is_some() {
                        return Err(UsageError("--on is given twice.".to_owned()));
                    }
                }
                _ => {
                    return Err(UsageError(format!(
                        "{name} is not an option of agents add."
                    )));
                }
            }
            continue;
        }
        match word.split_once('=') {
            Some((key, value)) if !key.is_empty() => {
                if pairs.contains_key(key) {
                    return Err(UsageError(format!("{key} is given twice.")));
                }
                pairs.insert(key.to_owned(), value_of(value));
            }
            _ => positional.push(word.clone()),
        }
    }
    let Some(agent) = positional.first().cloned() else {
        return Err(UsageError(
            "agents add takes an agent name: claude, codex, antigravity or opencode.".to_owned(),
        ));
    };
    if positional.len() > 2 {
        return Err(UsageError(
            "agents add takes the agent name and an optional principal.".to_owned(),
        ));
    }
    if pairs
        .insert("agent".to_owned(), Value::String(agent))
        .is_some()
    {
        return Err(UsageError("agent is given twice.".to_owned()));
    }
    if let Some(principal) = positional.get(1) {
        Principal::parse(principal)
            .map_err(|error| UsageError(format!("principal: {}", error.message)))?;
        if pairs
            .insert("as".to_owned(), Value::String(principal.clone()))
            .is_some()
        {
            return Err(UsageError("as is given twice.".to_owned()));
        }
    }
    Ok(Command::Call(CallCommand {
        tool: "agent.add".to_owned(),
        args: ArgsSource::Pairs(pairs),
        as_principal: None,
        on,
        json,
    }))
}

fn agents_remove(rest: &[String]) -> Result<Command, UsageError> {
    let mut pairs = Map::new();
    let mut positional = Vec::new();
    let mut as_principal = None;
    let mut on = None;
    let mut json = false;
    let mut words = rest.iter();
    while let Some(word) = words.next() {
        if word.starts_with('-') {
            let (name, inline) = match word.split_once('=') {
                Some((name, value)) => (name, Some(value.to_owned())),
                None => (word.as_str(), None),
            };
            let mut take_value = |opt: &str| {
                inline
                    .clone()
                    .or_else(|| words.next().cloned())
                    .ok_or_else(|| UsageError(format!("{opt} needs a value.")))
            };
            let no_inline = |opt: &str| {
                if inline.is_some() {
                    Err(UsageError(format!("{opt} takes no value.")))
                } else {
                    Ok(())
                }
            };
            match name {
                "--scope" => {
                    let scope = take_value(name)?;
                    if pairs
                        .insert("scope".to_owned(), Value::String(scope))
                        .is_some()
                    {
                        return Err(UsageError("scope is given twice.".to_owned()));
                    }
                }
                "--dry-run" => {
                    no_inline(name)?;
                    if pairs
                        .insert("dry_run".to_owned(), Value::Bool(true))
                        .is_some()
                    {
                        return Err(UsageError("dry_run is given twice.".to_owned()));
                    }
                }
                "--json" => {
                    no_inline(name)?;
                    json = true;
                }
                "--as" => {
                    let value = take_value(name)?;
                    Principal::parse(&value)
                        .map_err(|error| UsageError(format!("--as: {}", error.message)))?;
                    if as_principal.replace(value).is_some() {
                        return Err(UsageError("--as is given twice.".to_owned()));
                    }
                }
                "--on" => {
                    let value = take_value(name)?;
                    let target = Target::parse(&value)
                        .map_err(|error| UsageError(format!("--on: {error}.")))?;
                    if on.replace(target).is_some() {
                        return Err(UsageError("--on is given twice.".to_owned()));
                    }
                }
                _ => {
                    return Err(UsageError(format!(
                        "{name} is not an option of agents remove."
                    )));
                }
            }
            continue;
        }
        match word.split_once('=') {
            Some((key, value)) if !key.is_empty() => {
                if pairs.contains_key(key) {
                    return Err(UsageError(format!("{key} is given twice.")));
                }
                pairs.insert(key.to_owned(), value_of(value));
            }
            _ => positional.push(word.clone()),
        }
    }
    let [agent] = <[String; 1]>::try_from(positional).map_err(|_| {
        UsageError(
            "agents remove takes one agent name: claude, codex, antigravity or opencode."
                .to_owned(),
        )
    })?;
    if pairs
        .insert("agent".to_owned(), Value::String(agent))
        .is_some()
    {
        return Err(UsageError("agent is given twice.".to_owned()));
    }
    Ok(Command::Call(CallCommand {
        tool: "agent.remove".to_owned(),
        args: ArgsSource::Pairs(pairs),
        as_principal,
        on,
        json,
    }))
}

/// `legion token add|list|remove`, which is `token.add`, `token.list` and `token.remove` with the
/// names and the principal written as words rather than `key=value`.
fn token(rest: &[String]) -> Result<Command, UsageError> {
    let Some((verb, rest)) = rest.split_first() else {
        return Err(UsageError("token takes add, list or remove.".to_owned()));
    };
    let (tool, names): (&str, &[&str]) = match verb.as_str() {
        "add" => ("token.add", &["name", "principal"]),
        "list" => ("token.list", &[]),
        "remove" => ("token.remove", &["name"]),
        other => {
            return Err(UsageError(format!(
                "{other} is not something token does; it takes add, list or remove."
            )));
        }
    };
    let mut flags = Flags::read(rest, &["--json", "--as", "--on"])?;
    if flags.positional.len() != names.len() {
        return Err(UsageError(match names {
            [] => format!("token {verb} takes no names."),
            [name] => format!("token {verb} takes the token's {name}."),
            _ => format!("token {verb} takes the token's name and the principal it stands for."),
        }));
    }
    for (name, value) in names.iter().zip(flags.positional.drain(..)) {
        if flags
            .pairs
            .insert((*name).to_owned(), Value::String(value))
            .is_some()
        {
            return Err(UsageError(format!("{name} is given twice.")));
        }
    }
    Ok(Command::Call(CallCommand {
        tool: tool.to_owned(),
        args: ArgsSource::Pairs(flags.pairs),
        as_principal: flags.as_principal,
        on: flags.on,
        json: flags.json,
    }))
}

fn no_more(rest: &[String]) -> Result<(), UsageError> {
    match rest.first() {
        None => Ok(()),
        Some(extra) => Err(UsageError(format!("{extra} was not expected here."))),
    }
}

/// The options and `key=value` pairs after a command.
#[derive(Default)]
struct Flags {
    json: bool,
    compact: bool,
    as_principal: Option<String>,
    on: Option<Target>,
    args: Option<String>,
    pairs: Map<String, Value>,
    positional: Vec<String>,
}

impl Flags {
    fn read(rest: &[String], allowed: &[&str]) -> Result<Flags, UsageError> {
        let mut flags = Flags::default();
        let mut words = rest.iter();
        let mut options_ended = false;
        while let Some(word) = words.next() {
            if !options_ended && word == "--" {
                options_ended = true;
                continue;
            }
            if !options_ended && word.starts_with('-') {
                let (name, inline) = match word.split_once('=') {
                    Some((name, value)) => (name, Some(value.to_owned())),
                    None => (word.as_str(), None),
                };
                if !allowed.contains(&name) {
                    return Err(UsageError(format!("{name} is not an option here.")));
                }
                if name == "--json" || name == "--compact" {
                    if inline.is_some() {
                        return Err(UsageError(format!("{name} takes no value.")));
                    }
                    if name == "--json" {
                        flags.json = true;
                    } else {
                        flags.compact = true;
                    }
                    continue;
                }
                let value = match inline {
                    Some(value) => value,
                    None => words
                        .next()
                        .cloned()
                        .ok_or_else(|| UsageError(format!("{name} needs a value.")))?,
                };
                flags.set(name, value)?;
                continue;
            }
            match word.split_once('=') {
                Some((key, value)) if !key.is_empty() => {
                    if flags.pairs.contains_key(key) {
                        return Err(UsageError(format!("{key} is given twice.")));
                    }
                    flags.pairs.insert(key.to_owned(), value_of(value));
                }
                _ => flags.positional.push(word.clone()),
            }
        }
        Ok(flags)
    }

    fn set(&mut self, name: &str, value: String) -> Result<(), UsageError> {
        let twice = || UsageError(format!("{name} is given twice."));
        match name {
            "--as" => {
                Principal::parse(&value)
                    .map_err(|error| UsageError(format!("--as: {}", error.message)))?;
                if self.as_principal.replace(value).is_some() {
                    return Err(twice());
                }
            }
            "--on" => {
                let target =
                    Target::parse(&value).map_err(|error| UsageError(format!("--on: {error}.")))?;
                if self.on.replace(target).is_some() {
                    return Err(twice());
                }
            }
            _ => {
                if self.args.replace(value).is_some() {
                    return Err(twice());
                }
            }
        }
        Ok(())
    }

    fn no_pairs(&mut self, command: &str) -> Result<(), UsageError> {
        match self.pairs.keys().next().or(self.positional.first()) {
            None => Ok(()),
            Some(extra) => Err(UsageError(format!(
                "{command} takes no arguments, and {extra} was given."
            ))),
        }
    }
}

/// A `key=value` value: JSON when it parses as JSON, a string otherwise, so `n=3` is a number and
/// `path=/tmp` a string. `s="3"` makes the string "3".
pub fn value_of(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_owned()))
}

/// The arguments `--args` gave, which must be one JSON object.
pub fn json_object(text: &str, from: &str) -> Result<Value, UsageError> {
    match serde_json::from_str::<Value>(text) {
        Ok(value @ Value::Object(_)) => Ok(value),
        Ok(_) => Err(UsageError(format!(
            "{from} must be a JSON object, such as {{\"path\": \"~/notes.txt\"}}."
        ))),
        Err(error) => Err(UsageError(format!("{from} is not JSON: {error}."))),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn run(line: &[&str]) -> Result<Command, UsageError> {
        parse(line.iter().map(OsString::from))
    }

    fn call(line: &[&str]) -> CallCommand {
        match run(line) {
            Ok(Command::Call(call)) => call,
            other => panic!("{line:?} gave {other:?}"),
        }
    }

    #[test]
    fn values_are_json_when_they_parse_and_strings_otherwise() {
        let parsed = call(&[
            "test.echo",
            "n=42",
            "ok=true",
            "none=null",
            "list=[1,\"a\"]",
            "obj={\"a\":1}",
            "path=/tmp/x",
            "quoted=\"7\"",
            "empty=",
            "eq=a=b",
            "word=hello world",
        ]);
        let ArgsSource::Pairs(pairs) = parsed.args else {
            panic!("{:?}", parsed.args)
        };
        assert_eq!(
            Value::Object(pairs),
            json!({
                "n": 42, "ok": true, "none": null, "list": [1, "a"], "obj": {"a": 1}, "path": "/tmp/x",
                "quoted": "7", "empty": "", "eq": "a=b", "word": "hello world"
            })
        );
    }

    #[test]
    fn options_go_anywhere_after_the_tool() {
        let parsed = call(&[
            "meta.whoami",
            "--as",
            "agent:claude",
            "a=1",
            "--json",
            "--on=tower,pi",
        ]);
        assert_eq!(parsed.as_principal.as_deref(), Some("agent:claude"));
        assert!(parsed.json);
        assert_eq!(
            parsed.on,
            Some(Target::Many(vec!["tower".into(), "pi".into()]))
        );
        let parsed = call(&["test.echo", "--", "--odd=1"]);
        assert_eq!(
            parsed.args,
            ArgsSource::Pairs(json!({"--odd": 1}).as_object().cloned().unwrap_or_default())
        );
    }

    #[test]
    fn args_is_the_exact_alternative() {
        assert_eq!(
            call(&["test.echo", "--args", "{\"a\":1}"]).args,
            ArgsSource::Json("{\"a\":1}".into())
        );
        assert_eq!(call(&["test.echo", "--args=-"]).args, ArgsSource::Stdin);
        assert!(run(&["test.echo", "--args", "{}", "a=1"]).is_err());
        assert!(json_object("[1]", "--args").is_err());
        assert!(json_object("{", "--args").is_err());
        assert_eq!(json_object("{\"a\":1}", "--args"), Ok(json!({"a": 1})));
    }

    #[test]
    fn mistakes_are_usage_errors() {
        for line in [
            &[][..],
            &["frob"],
            &["--frob"],
            &["meta.version", "--frob"],
            &["meta.version", "stray"],
            &["meta.version", "a=1", "a=2"],
            &["meta.version", "--as"],
            &["meta.version", "--as", "not ok"],
            &["meta.version", "--as", "a", "--as", "b"],
            &["meta.version", "--on", ""],
            &["meta.version", "--json=yes"],
            &["tools", "a=1"],
            &["rpc", "--json"],
            &["rpc", "--compact"],
            &["mcp", "--json"],
            &["mcp", "--compact=yes"],
            &["mcp", "a=1"],
            &["help", "a.b", "c.d"],
            &["--version", "x"],
            &["gate"],
            &["gate", "--as", "a", "extra"],
            &["stream", "x=1"],
            &["cp", "a"],
            &["cp", "a", "b", "c"],
            &["cp", "a", "b", "--frob"],
            &["cp", "a", "b", "--delete=yes"],
            &["cp", "a", "b", "--exclude"],
            &["cp", "a", "b", "--as", "not ok"],
        ] {
            assert!(run(line).is_err(), "{line:?} was accepted");
        }
    }

    #[test]
    fn commands() {
        assert_eq!(run(&["tools"]), Ok(Command::Tools { json: false }));
        assert_eq!(run(&["tools", "--json"]), Ok(Command::Tools { json: true }));
        assert_eq!(
            run(&["help"]),
            Ok(Command::Help {
                tool: None,
                json: false
            })
        );
        assert_eq!(
            run(&["help", "meta.version", "--json"]),
            Ok(Command::Help {
                tool: Some("meta.version".into()),
                json: true
            })
        );
        assert_eq!(
            run(&["rpc", "--as", "phone"]),
            Ok(Command::Rpc {
                pinned: Some("phone".into())
            })
        );
        assert_eq!(run(&["rpc"]), Ok(Command::Rpc { pinned: None }));
        assert_eq!(
            run(&["mcp", "--compact", "--as", "agent:claude"]),
            Ok(Command::Mcp {
                pinned: Some("agent:claude".into()),
                compact: true
            })
        );
        assert_eq!(
            run(&["mcp"]),
            Ok(Command::Mcp {
                pinned: None,
                compact: false
            })
        );
        assert_eq!(
            run(&["stream", "--as", "phone"]),
            Ok(Command::Stream {
                pinned: Some("phone".into())
            })
        );
        assert_eq!(
            run(&["gate", "--as", "agent:example"]),
            Ok(Command::Gate(GateCommand::As {
                pinned: "agent:example".into(),
                device: None,
                account: None,
                home: GateHome::Default,
            }))
        );
        let abs_home = std::env::temp_dir().join("legion-gate-home");
        let abs_home_str = abs_home.to_string_lossy().into_owned();
        let abs_cfg = std::env::temp_dir().join("legion-gate-cfg");
        let abs_state = std::env::temp_dir().join("legion-gate-state");
        let abs_cfg_str = abs_cfg.to_string_lossy().into_owned();
        let abs_state_str = abs_state.to_string_lossy().into_owned();
        assert_eq!(
            run(&[
                "gate",
                "--as",
                "agent:example",
                "--device",
                "pixel",
                "--account",
                "me",
                "--home",
                &abs_home_str
            ]),
            Ok(Command::Gate(GateCommand::As {
                pinned: "agent:example".into(),
                device: Some("pixel".into()),
                account: Some("me".into()),
                home: GateHome::Root(abs_home.clone()),
            }))
        );
        assert_eq!(
            run(&[
                "gate",
                "--as",
                "agent:example",
                "--device",
                "pixel",
                "--config-dir",
                &abs_cfg_str,
                "--state-dir",
                &abs_state_str,
            ]),
            Ok(Command::Gate(GateCommand::As {
                pinned: "agent:example".into(),
                device: Some("pixel".into()),
                account: None,
                home: GateHome::Split {
                    config_dir: abs_cfg,
                    state_dir: abs_state,
                },
            }))
        );
        assert_eq!(
            run(&["gate", "--enrol", "0123456789abcdef0123456789abcdef"]),
            Ok(Command::Gate(GateCommand::Enrol {
                id: "0123456789abcdef0123456789abcdef".into(),
                account: None,
                home: GateHome::Default,
            }))
        );
        assert_eq!(
            run(&[
                "gate",
                "--enrol",
                "0123456789abcdef0123456789abcdef",
                "--account",
                "me",
                "--home",
                &abs_home_str,
            ]),
            Ok(Command::Gate(GateCommand::Enrol {
                id: "0123456789abcdef0123456789abcdef".into(),
                account: Some("me".into()),
                home: GateHome::Root(abs_home),
            }))
        );
        for bad_gate in [
            &["gate", "--as", "phone", "--enrol", "0123456789abcdef"][..],
            &["gate", "--as", "phone", "--home", "relative/path"],
            &["gate", "--as", "phone", "--config-dir", &abs_cfg_str],
            &["gate", "--as", "phone", "--state-dir", &abs_state_str],
            &[
                "gate",
                "--as",
                "phone",
                "--home",
                &abs_home_str,
                "--config-dir",
                &abs_cfg_str,
                "--state-dir",
                &abs_state_str,
            ],
            &[
                "gate",
                "--as",
                "phone",
                "--config-dir",
                "relative",
                "--state-dir",
                &abs_state_str,
            ],
            &["gate", "--as", "phone", "--device", "../escape"],
            &["gate", "--as", "phone", "--account", "-bad"],
            &["gate", "--as", "phone", "--account", "has space"],
            &[
                "gate",
                "--enrol",
                "0123456789abcdef0123456789abcdef",
                "--device",
                "pixel",
            ],
            &["gate", "--enrol", "bad/id"],
            &["gate", "--enrol"],
            &["gate", "--unknown"],
        ] {
            assert!(run(bad_gate).is_err(), "{bad_gate:?}");
        }
        assert_eq!(
            run(&["pair", "--as", "phone"]),
            Ok(Command::Pair(PairCommand {
                principal: "phone".into(),
                valid_minutes: None,
                hosts: Vec::new(),
                port: None,
                user: None,
                allow_owner: false,
                invert: false,
                on: None,
                json: false,
            }))
        );
        assert_eq!(
            run(&[
                "pair",
                "--as=phone",
                "--valid",
                "1h30m",
                "--host",
                "tower.example",
                "--host=100.64.0.10",
                "--port",
                "2222",
                "--user",
                "me",
                "--allow-owner",
                "--invert",
                "--on",
                "pi",
                "--json",
            ]),
            Ok(Command::Pair(PairCommand {
                principal: "phone".into(),
                valid_minutes: Some(90),
                hosts: vec!["tower.example".into(), "100.64.0.10".into()],
                port: Some(2222),
                user: Some("me".into()),
                allow_owner: true,
                invert: true,
                on: Some("pi".into()),
                json: true,
            }))
        );
        assert_eq!(parse_valid_minutes("15"), Ok(15));
        assert_eq!(parse_valid_minutes("15m"), Ok(15));
        assert_eq!(parse_valid_minutes("1h"), Ok(60));
        assert_eq!(parse_valid_minutes("24h"), Ok(1440));
        assert_eq!(parse_valid_minutes("1d"), Ok(1440));
        assert_eq!(command_help("pair"), Some(PAIR_HELP));
        for bad_pair in [
            &["pair"][..],
            &["pair", "phone"],
            &["pair", "--as", "phone", "--valid", "0m"],
            &["pair", "--as", "phone", "--valid", "25h"],
            &["pair", "--as", "phone", "--valid", "30s"],
            &["pair", "--as", "phone", "--port", "0"],
            &["pair", "--as", "phone", "--on", "all"],
            &["pair", "--as", "phone", "--on", "tower,pi"],
            &["pair", "--as", "phone", "--invert=yes"],
            &["pair", "--as", "phone", "--unknown"],
        ] {
            assert!(run(bad_pair).is_err(), "{bad_pair:?}");
        }
        assert_eq!(
            run(&["cp", "pi:~/app", "--checksum", "--", "--odd-name"]),
            Ok(Command::Cp(CpCommand {
                from: "pi:~/app".into(),
                to: "--odd-name".into(),
                delete: false,
                checksum: true,
                exclude: Vec::new(),
                dry_run: false,
                as_principal: None,
                json: false,
            }))
        );
        assert_eq!(
            run(&[
                "cp",
                "--exclude",
                "*.log",
                "pi:~/app",
                "--delete",
                "C:\\Work\\app",
                "--exclude=target/",
                "--dry-run",
                "--as",
                "agent:example",
                "--json",
            ]),
            Ok(Command::Cp(CpCommand {
                from: "pi:~/app".into(),
                to: "C:\\Work\\app".into(),
                delete: true,
                checksum: false,
                exclude: vec!["*.log".into(), "target/".into()],
                dry_run: true,
                as_principal: Some("agent:example".into()),
                json: true,
            }))
        );
        assert_eq!(command_help("gate"), Some(GATE_HELP));
        assert!(
            GATE_HELP.contains(
                "command=\"/home/me/.local/bin/legion gate --as agent:example\",restrict"
            )
        );
        assert_eq!(command_help("fs.read"), None);
        assert_eq!(run(&["--version"]), Ok(Command::Version));
        assert_eq!(
            run(&["-h"]),
            Ok(Command::Help {
                tool: None,
                json: false
            })
        );
    }

    #[test]
    fn serve_and_token_words() {
        assert_eq!(
            run(&["serve"]),
            Ok(Command::Serve {
                listen: None,
                help: false
            })
        );
        assert_eq!(
            run(&["serve", "--listen", "127.0.0.1:0"]),
            Ok(Command::Serve {
                listen: Some("127.0.0.1:0".into()),
                help: false
            })
        );
        assert_eq!(
            run(&["serve", "--listen=[::1]:7117", "--help"]),
            Ok(Command::Serve {
                listen: Some("[::1]:7117".into()),
                help: true
            })
        );
        assert!(run(&["serve", "--listen"]).is_err());
        assert!(run(&["serve", "--listen", "a", "--listen", "b"]).is_err());
        assert!(run(&["serve", "--port", "1"]).is_err());

        let add = call(&[
            "token",
            "add",
            "phone",
            "phone",
            "expires_days=30",
            "--json",
        ]);
        assert_eq!(add.tool, "token.add");
        assert!(add.json);
        assert_eq!(
            add.args,
            ArgsSource::Pairs(
                json!({"expires_days": 30, "name": "phone", "principal": "phone"})
                    .as_object()
                    .cloned()
                    .unwrap_or_default()
            )
        );
        assert_eq!(call(&["token", "list"]).tool, "token.list");
        let remove = call(&["token", "remove", "phone", "--on", "pi"]);
        assert_eq!(remove.tool, "token.remove");
        assert_eq!(remove.on, Some(Target::One("pi".into())));
        for wrong in [
            &["token"][..],
            &["token", "make"],
            &["token", "add", "phone"],
            &["token", "list", "x"],
            &["token", "remove", "phone", "name=other"],
        ] {
            assert!(run(wrong).is_err(), "{wrong:?}");
        }

        let init_plain = call(&["init"]);
        assert_eq!(init_plain.tool, "meta.init");
        assert_eq!(init_plain.args, ArgsSource::Pairs(Map::new()));

        let init_opts = call(&[
            "init",
            "--name",
            "tower",
            "--no-roles",
            "--dry-run",
            "--on",
            "pi",
            "--json",
        ]);
        assert_eq!(init_opts.tool, "meta.init");
        assert!(init_opts.json);
        assert_eq!(init_opts.on, Some(Target::One("pi".into())));
        assert_eq!(
            init_opts.args,
            ArgsSource::Pairs(
                json!({"name": "tower", "roles": false, "dry_run": true})
                    .as_object()
                    .cloned()
                    .unwrap_or_default()
            )
        );
        assert_eq!(command_help("init"), Some(INIT_HELP));
        for wrong in [
            &["init", "stray"][..],
            &["init", "--name"],
            &["init", "--dry-run=yes"],
            &["init", "--unknown"],
        ] {
            assert!(run(wrong).is_err(), "{wrong:?}");
        }

        assert_eq!(call(&["agents", "list"]).tool, "agent.list");
        let add_agent = call(&[
            "agents",
            "add",
            "claude",
            "--as",
            "agent:claude",
            "--full",
            "--scope",
            "project",
            "--dry-run",
            "--json",
        ]);
        assert_eq!(add_agent.tool, "agent.add");
        assert!(add_agent.json);
        assert_eq!(add_agent.as_principal, None);
        assert_eq!(
            add_agent.args,
            ArgsSource::Pairs(
                json!({
                    "agent": "claude",
                    "as": "agent:claude",
                    "compact": false,
                    "scope": "project",
                    "dry_run": true
                })
                .as_object()
                .cloned()
                .unwrap_or_default()
            )
        );
        let remove_agent = call(&["agents", "remove", "codex", "--dry-run", "--on", "tower"]);
        assert_eq!(remove_agent.tool, "agent.remove");
        assert_eq!(remove_agent.on, Some(Target::One("tower".into())));
        assert_eq!(command_help("agents"), Some(AGENTS_HELP));
        for wrong in [
            &["agents"][..],
            &["agents", "sync"],
            &["agents", "list", "extra"],
            &["agents", "add"],
            &["agents", "add", "claude", "--as"],
            &["agents", "remove"],
            &["agents", "remove", "claude", "extra"],
        ] {
            assert!(run(wrong).is_err(), "{wrong:?}");
        }
    }
}
