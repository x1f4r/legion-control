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
                if name == "--json" {
                    if inline.is_some() {
                        return Err(UsageError("--json takes no value.".to_owned()));
                    }
                    flags.json = true;
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
        assert_eq!(run(&["--version"]), Ok(Command::Version));
        assert_eq!(
            run(&["-h"]),
            Ok(Command::Help {
                tool: None,
                json: false
            })
        );
    }
}
