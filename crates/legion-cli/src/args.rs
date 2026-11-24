//! The command line, read into a [`Command`] before anything runs, so a mistake in it is always a
//! usage error and never a half-done call.

use std::ffi::OsString;
use std::fmt;

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
        /// Serve three tools that list, describe and call the others.
        compact: bool,
    },
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
  legion rpc [--as PRINCIPAL]
  legion mcp [--as PRINCIPAL] [--compact]
  legion serve [--listen ADDRESS]
  legion token add NAME PRINCIPAL [expires_days=N] [allow_owner=true] | list | remove NAME
  legion --version

A value that parses as JSON is JSON (n=3, ok=true, tags=[\"a\"]); anything else is a string.
--args takes the arguments as one JSON object instead, or reads it from stdin when given -.
Output is readable on a terminal and JSON otherwise; --json asks for JSON anywhere.";

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
        "serve" => serve(rest),
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
    }
}
