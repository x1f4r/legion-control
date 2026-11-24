//! Example CLI driving `legion-link` over in-process SSH.
//!
//! Subcommands:
//! - `link keygen <file>`
//! - `link enrol [--invite-file <path|->] --key <file> --name <name> --out <config.json>`
//! - `link tools --config <config.json> [--node N] [--on M]`
//! - `link call --config <config.json> [--node N] [--on M] <tool> '<json args>'`
//!
//! Invites carry a secret temporary key and are therefore read from `--invite-file <path>` or
//! stdin (`--invite-file -`, the default), never from a command-line argument.
//!
//! Prints exactly one JSON document to stdout per run. Exit codes: 0 on success, 1 on link/node
//! error, 2 on usage error.

use std::fs;
use std::io::{self, Read, Write};
use std::path::Path;
use std::process::ExitCode;

use legion_link::proto::{ErrorBody, ErrorCode, Target};
use legion_link::{
    CancellationToken, DeviceKey, Invite, Link, LinkConfig, MAX_INVITE_LEN, NodeLink, SshTimeouts,
    enrol,
};
use serde::Serialize;
use serde_json::{Value, json};

const FAILED: u8 = 1;
const USAGE: u8 = 2;

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args).await {
        Ok(value) => {
            print_json(&value);
            ExitCode::SUCCESS
        }
        Err(RunError::Usage(err)) => {
            print_json(&err);
            ExitCode::from(USAGE)
        }
        Err(RunError::Failed(err)) => {
            print_json(&err);
            ExitCode::from(FAILED)
        }
    }
}

enum RunError {
    Usage(ErrorBody),
    Failed(ErrorBody),
}

async fn run(args: &[String]) -> Result<Value, RunError> {
    let Some((sub, rest)) = args.split_first() else {
        return Err(usage(
            "Expected a subcommand: `keygen`, `enrol`, `tools`, or `call`.",
        ));
    };
    match sub.as_str() {
        "keygen" => cmd_keygen(rest),
        "enrol" => cmd_enrol(rest).await,
        "tools" => cmd_tools(rest).await,
        "call" => cmd_call(rest).await,
        other => Err(usage(&format!(
            "Unknown subcommand {other:?}; expected `keygen`, `enrol`, `tools`, or `call`."
        ))),
    }
}

fn cmd_keygen(args: &[String]) -> Result<Value, RunError> {
    let [path] = args else {
        return Err(usage("Usage: link keygen <file>"));
    };
    let key = DeviceKey::generate().map_err(RunError::Failed)?;
    let pem = key.to_openssh().map_err(RunError::Failed)?;
    let public_key = key.public_key_line("").map_err(RunError::Failed)?;
    let fingerprint = key.fingerprint();
    write_secret_file(Path::new(path), pem.as_bytes()).map_err(RunError::Failed)?;
    Ok(json!({
        "path": path,
        "public_key": public_key,
        "fingerprint": fingerprint,
    }))
}

async fn cmd_enrol(args: &[String]) -> Result<Value, RunError> {
    let mut invite_file: Option<String> = None;
    let mut key_path: Option<String> = None;
    let mut name: Option<String> = None;
    let mut out_path: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--invite-file" => {
                invite_file = Some(next_value(args, &mut i, "--invite-file")?);
            }
            "--key" => {
                key_path = Some(next_value(args, &mut i, "--key")?);
            }
            "--name" => {
                name = Some(next_value(args, &mut i, "--name")?);
            }
            "--out" => {
                out_path = Some(next_value(args, &mut i, "--out")?);
            }
            "-" => {
                invite_file = Some("-".to_owned());
            }
            other if other.starts_with('-') => {
                return Err(usage(&format!("Unknown option {other:?} for `enrol`.")));
            }
            _ => {
                return Err(usage(
                    "Do not pass the invite text as a command-line argument (it carries a secret key). Use `--invite-file <path>` or pipe it on stdin (`--invite-file -`).",
                ));
            }
        }
        i += 1;
    }

    let key_path = key_path.ok_or_else(|| usage("`enrol` requires `--key <file>`."))?;
    let name = name.ok_or_else(|| usage("`enrol` requires `--name <name>`."))?;
    let out_path = out_path.ok_or_else(|| usage("`enrol` requires `--out <config.json>`."))?;

    let invite_text = read_invite(invite_file.as_deref())?;
    let invite = Invite::parse(&invite_text).map_err(RunError::Failed)?;
    let key_pem = fs::read_to_string(&key_path).map_err(|err| {
        RunError::Failed(
            ErrorBody::new(
                ErrorCode::NotFound,
                format!("Could not read device key {key_path:?}: {err}."),
            )
            .with_hint("Create a device key first with `link keygen <file>`."),
        )
    })?;
    let device_key = DeviceKey::from_openssh(&key_pem).map_err(RunError::Failed)?;

    let enrolled = enrol(
        &invite,
        &device_key,
        &name,
        SshTimeouts::default(),
        CancellationToken::new(),
    )
    .await
    .map_err(RunError::Failed)?;

    let out = Path::new(&out_path);
    let mut config = if out.exists() {
        load_config(&out_path)?
    } else {
        LinkConfig::default()
    };
    config.device_key = Some(device_key.to_openssh().map_err(RunError::Failed)?);
    config.upsert_node(NodeLink::from(enrolled.clone()));
    let config_json = serde_json::to_string_pretty(&config).map_err(|err| {
        RunError::Failed(ErrorBody::new(
            ErrorCode::Internal,
            format!("Could not serialize link config: {err}."),
        ))
    })?;
    write_secret_file(out, format!("{config_json}\n").as_bytes()).map_err(RunError::Failed)?;

    serde_json::to_value(&enrolled).map_err(|err| {
        RunError::Failed(ErrorBody::new(
            ErrorCode::Internal,
            format!("Could not serialize enrolment result: {err}."),
        ))
    })
}

async fn cmd_tools(args: &[String]) -> Result<Value, RunError> {
    let mut config_path: Option<String> = None;
    let mut node_name: Option<String> = None;
    let mut on: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--config" => config_path = Some(next_value(args, &mut i, "--config")?),
            "--node" => node_name = Some(next_value(args, &mut i, "--node")?),
            "--on" => on = Some(next_value(args, &mut i, "--on")?),
            other => {
                return Err(usage(&format!(
                    "Unexpected argument {other:?}. Usage: link tools --config <config.json> [--node N] [--on M]"
                )));
            }
        }
        i += 1;
    }

    let config_path =
        config_path.ok_or_else(|| usage("`tools` requires `--config <config.json>`."))?;
    let config = load_config(&config_path)?;
    let node = pick_node(&config, node_name.as_deref())?;
    let on_val = parse_cli_on(on.as_deref())?;

    let link = Link::new(config);
    let result = link.tools(&node, on_val).await;
    link.close().await;
    result.map_err(RunError::Failed)
}

async fn cmd_call(args: &[String]) -> Result<Value, RunError> {
    let mut config_path: Option<String> = None;
    let mut node_name: Option<String> = None;
    let mut on: Option<String> = None;
    let mut positional: Vec<String> = Vec::new();

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--config" => config_path = Some(next_value(args, &mut i, "--config")?),
            "--node" => node_name = Some(next_value(args, &mut i, "--node")?),
            "--on" => on = Some(next_value(args, &mut i, "--on")?),
            other if other.starts_with('-') => {
                return Err(usage(&format!("Unknown option {other:?} for `call`.")));
            }
            arg => positional.push(arg.to_owned()),
        }
        i += 1;
    }

    let config_path =
        config_path.ok_or_else(|| usage("`call` requires `--config <config.json>`."))?;
    let (tool, args_json) = match positional.as_slice() {
        [tool] => (tool.clone(), "{}".to_owned()),
        [tool, raw_args] => (tool.clone(), raw_args.clone()),
        _ => {
            return Err(usage(
                "Usage: link call --config <config.json> <tool> '<json args>' [--node N] [--on M]",
            ));
        }
    };

    let tool_args: Value = serde_json::from_str(&args_json)
        .map_err(|err| usage(&format!("Tool arguments must be valid JSON: {err}.")))?;
    let on_val = parse_cli_on(on.as_deref())?;
    let config = load_config(&config_path)?;
    let node = pick_node(&config, node_name.as_deref())?;

    let link = Link::new(config);
    let result = link
        .call(&node, &tool, tool_args, on_val, |event| {
            let _ = writeln!(io::stderr().lock(), "{event}");
        })
        .await;
    link.close().await;
    result.map_err(RunError::Failed)
}

fn read_invite(path: Option<&str>) -> Result<String, RunError> {
    let limit = (MAX_INVITE_LEN + 64) as u64;
    match path {
        None | Some("-") => {
            let mut buf = String::new();
            io::stdin()
                .take(limit)
                .read_to_string(&mut buf)
                .map_err(|err| {
                    RunError::Failed(ErrorBody::new(
                        ErrorCode::Failed,
                        format!("Could not read invite from stdin: {err}."),
                    ))
                })?;
            Ok(buf)
        }
        Some(file) => {
            let f = fs::File::open(file).map_err(|err| {
                RunError::Failed(
                    ErrorBody::new(
                        ErrorCode::NotFound,
                        format!("Could not open invite file {file:?}: {err}."),
                    )
                    .with_hint("Pass `--invite-file <path>` or pipe the invite on stdin."),
                )
            })?;
            let mut buf = String::new();
            f.take(limit).read_to_string(&mut buf).map_err(|err| {
                RunError::Failed(ErrorBody::new(
                    ErrorCode::Failed,
                    format!("Could not read invite file {file:?}: {err}."),
                ))
            })?;
            Ok(buf)
        }
    }
}

fn next_value(args: &[String], index: &mut usize, flag: &str) -> Result<String, RunError> {
    *index += 1;
    args.get(*index)
        .cloned()
        .ok_or_else(|| usage(&format!("Option `{flag}` requires a value.")))
}

fn parse_cli_on(raw: Option<&str>) -> Result<Option<Value>, RunError> {
    let Some(text) = raw else {
        return Ok(None);
    };
    let target = Target::parse(text).map_err(|err| usage(&format!("--on: {err}.")))?;
    let value = serde_json::to_value(target).map_err(|err| {
        RunError::Failed(ErrorBody::new(
            ErrorCode::Internal,
            format!("Could not encode target: {err}."),
        ))
    })?;
    Ok(Some(value))
}

fn load_config(path: &str) -> Result<LinkConfig, RunError> {
    let text = fs::read_to_string(path).map_err(|err| {
        RunError::Failed(
            ErrorBody::new(
                ErrorCode::NotFound,
                format!("Could not read config {path:?}: {err}."),
            )
            .with_hint("Pass the config file written by `link enrol`."),
        )
    })?;
    serde_json::from_str(&text).map_err(|err| {
        RunError::Failed(
            ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!("Config {path:?} is not valid JSON: {err}."),
            )
            .with_hint("Check the config JSON structure."),
        )
    })
}

fn pick_node(config: &LinkConfig, explicit: Option<&str>) -> Result<String, RunError> {
    if let Some(name) = explicit {
        return Ok(name.to_owned());
    }
    match config.nodes.as_slice() {
        [only] => Ok(only.node.clone()),
        [] => Err(RunError::Failed(
            ErrorBody::new(ErrorCode::NotFound, "Config contains no paired nodes.")
                .with_hint("Pair a node with `link enrol` first."),
        )),
        _ => Err(usage(
            "Config lists multiple nodes; pass `--node <name>` to choose one.",
        )),
    }
}

fn write_secret_file(path: &Path, bytes: &[u8]) -> Result<(), ErrorBody> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|err| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!("Could not create directory {}: {err}.", parent.display()),
            )
        })?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .map_err(|err| {
                ErrorBody::new(
                    ErrorCode::Failed,
                    format!("Could not write {}: {err}.", path.display()),
                )
            })?;
        file.write_all(bytes).map_err(|err| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!("Could not write {}: {err}.", path.display()),
            )
        })?;
        file.flush().map_err(|err| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!("Could not flush {}: {err}.", path.display()),
            )
        })?;
    }
    #[cfg(not(unix))]
    {
        fs::write(path, bytes).map_err(|err| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!("Could not write {}: {err}.", path.display()),
            )
        })?;
    }
    Ok(())
}

fn usage(message: &str) -> RunError {
    RunError::Usage(ErrorBody::new(ErrorCode::InvalidArgs, message))
}

fn print_json<T: Serialize>(value: &T) {
    if let Ok(text) = serde_json::to_string(value) {
        let mut stdout = io::stdout().lock();
        let _ = stdout.write_all(text.as_bytes());
        let _ = stdout.write_all(b"\n");
        let _ = stdout.flush();
    }
}
