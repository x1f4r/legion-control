//! `legion pair`: terminal-friendly wrapper over `device.invite` with role/name prompts and a
//! Unicode half-block QR code.

use std::fmt::Write as _;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::Path;
use std::process::ExitCode;

use legion_core::config::parse;
use legion_core::policy::{OWNER, Policy, Role, Rule};
use legion_core::proto::{Effect, ErrorBody, ErrorCode, Target};
use legion_core::{Call, CancellationToken, Door, Node, RouteOutcome, Router};
use qrcode::render::unicode::Dense1x2;
use qrcode::{EcLevel, QrCode};
use serde_json::{Map, Value, json};

use crate::args::PairCommand;
use crate::{FAILED, err, out, print_json, render, run_call};

/// Calls `device.invite` (asking interactively for the device name and role on a terminal when
/// neither `--as` nor `--name`/`--role` was provided) and prints either raw JSON (`--json`) or
/// human-readable pairing output containing summary lines, a terminal QR code, the one-line invite
/// text, and management commands.
pub async fn run(
    node: &Node,
    router: &dyn Router,
    command: PairCommand,
    shutdown: CancellationToken,
) -> ExitCode {
    let mut allow_owner = command.allow_owner;
    let on_name = command.on.clone();
    let on = command.on.map(Target::One);

    let (principal, role, device_name) = if let Some(principal) = command.principal {
        (Some(principal), None, None)
    } else if let (Some(role), Some(name)) = (command.role.clone(), command.name.clone()) {
        (None, Some(role), Some(name))
    } else {
        if command.json || !io::stdin().is_terminal() || !io::stderr().is_terminal() {
            let error = ErrorBody::new(
                ErrorCode::InvalidArgs,
                "legion pair needs --name NAME and --role ROLE (or --as PRINCIPAL) when not running on a terminal.",
            )
            .with_hint("Run `legion pair` interactively on a terminal, or pass `--name <device> --role <role>` (or `--as <principal>`).");
            if command.json {
                print_json(&error, true);
            } else {
                err(&render::error(&error));
            }
            return ExitCode::from(FAILED);
        }
        let policy = match load_target_policy(node, router, on.clone(), shutdown.clone()).await {
            Ok(policy) => policy,
            Err(error) => {
                err(&render::error(&error));
                return ExitCode::from(FAILED);
            }
        };
        let stdin = io::stdin();
        let mut reader = stdin.lock();
        let mut stderr = io::stderr().lock();
        match prompt_pair_choices(
            &policy,
            command.name.as_deref(),
            command.role.as_deref(),
            allow_owner,
            &mut reader,
            &mut stderr,
        ) {
            Ok((chosen_name, chosen_role, confirmed_owner)) => {
                allow_owner = confirmed_owner;
                (None, Some(chosen_role), Some(chosen_name))
            }
            Err(error) => {
                err(&render::error(&error));
                return ExitCode::from(FAILED);
            }
        }
    };

    let mut args = Map::new();
    if let Some(principal) = principal {
        args.insert("principal".to_owned(), Value::String(principal));
    }
    if let Some(role) = role {
        args.insert("role".to_owned(), Value::String(role));
    }
    if let Some(device_name) = device_name {
        args.insert("name".to_owned(), Value::String(device_name));
    }
    if let Some(valid_minutes) = command.valid_minutes {
        args.insert("valid_minutes".to_owned(), Value::from(valid_minutes));
    }
    if !command.hosts.is_empty() {
        args.insert(
            "host".to_owned(),
            Value::Array(command.hosts.into_iter().map(Value::String).collect()),
        );
    }
    if let Some(port) = command.port {
        args.insert("port".to_owned(), Value::from(port));
    }
    if let Some(user) = command.user {
        args.insert("user".to_owned(), Value::String(user));
    }
    if allow_owner {
        args.insert("allow_owner".to_owned(), Value::Bool(true));
    }

    let call = Call::new(Door::Cli, "device.invite", Value::Object(args));
    let outcome = run_call(node, router, call, on, shutdown).await;

    match outcome {
        RouteOutcome::One(Ok(value)) if command.json => {
            print_json(&value, true);
            ExitCode::SUCCESS
        }
        RouteOutcome::One(Ok(value)) => {
            match format_human(&value, command.invert, on_name.as_deref()) {
                Ok(text) => {
                    out(&text);
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    err(&render::error(&error));
                    ExitCode::from(FAILED)
                }
            }
        }
        RouteOutcome::One(Err(error)) => {
            if command.json {
                print_json(&error, true);
            } else {
                err(&render::error(&error));
            }
            ExitCode::from(FAILED)
        }
        RouteOutcome::Many(_) => {
            let error = ErrorBody::new(
                ErrorCode::Internal,
                "pair unexpectedly received a multi-machine result.",
            );
            if command.json {
                print_json(&error, true);
            } else {
                err(&render::error(&error));
            }
            ExitCode::from(FAILED)
        }
    }
}

async fn load_target_policy(
    node: &Node,
    router: &dyn Router,
    on: Option<Target>,
    shutdown: CancellationToken,
) -> Result<Policy, ErrorBody> {
    if on.is_none() {
        return node.config().get::<Policy>().map(|arc| (*arc).clone());
    }
    let call = Call::new(Door::Cli, "config.read", json!({"document": "policy.toml"}));
    match run_call(node, router, call, on, shutdown).await {
        RouteOutcome::One(Ok(value)) => {
            let Some(content) = value.get("content").and_then(Value::as_str) else {
                return Ok(Policy::default());
            };
            parse::<Policy>(Path::new("policy.toml"), content.as_bytes())
        }
        RouteOutcome::One(Err(error)) => Err(error),
        RouteOutcome::Many(_) => Err(ErrorBody::new(
            ErrorCode::Internal,
            "pair unexpectedly received a multi-machine policy response.",
        )),
    }
}

/// Returns a concise one-line plain-English description of what `role` may do, derived directly
/// from its rules in `policy.toml` (with no hardcoded role names).
pub fn describe_role(name: &str, role: &Role) -> String {
    if name == OWNER {
        return "everything (full access, including pairing and removing devices)".to_owned();
    }
    let allows_effect = |effect: Effect| -> bool {
        let covers = |rule: &Rule| match rule {
            Rule::Effect(of) => *of == effect,
            Rule::Tools(pattern) => pattern.chars().all(|c| c == '*'),
        };
        !role.deny.iter().any(covers) && role.allow.iter().any(covers)
    };

    if allows_effect(Effect::Admin) {
        "administer (full control over tools and settings)".to_owned()
    } else if allows_effect(Effect::Exec) {
        "operate (read, change files, and run commands — as strong as this account)".to_owned()
    } else if allows_effect(Effect::Write) {
        "change files (read and edit files, no running commands)".to_owned()
    } else if allows_effect(Effect::Read)
        && !allows_effect(Effect::Destroy)
        && !allows_effect(Effect::Power)
    {
        "look only (read status and files, change nothing)".to_owned()
    } else if role.allow.is_empty() {
        "nothing (no tools allowed)".to_owned()
    } else {
        let allowed: Vec<String> = role.allow.iter().take(4).map(ToString::to_string).collect();
        let suffix = if role.allow.len() > 4 { ", ..." } else { "" };
        format!("custom ({}{suffix})", allowed.join(", "))
    }
}

fn role_rank(name: &str, role: &Role) -> u8 {
    if name == OWNER {
        return 5;
    }
    let allows = |effect: Effect| -> bool {
        let covers = |rule: &Rule| match rule {
            Rule::Effect(of) => *of == effect,
            Rule::Tools(pattern) => pattern.chars().all(|c| c == '*'),
        };
        !role.deny.iter().any(covers) && role.allow.iter().any(covers)
    };
    if allows(Effect::Admin) {
        4
    } else if allows(Effect::Exec) {
        3
    } else if allows(Effect::Write) {
        2
    } else if allows(Effect::Read) {
        1
    } else {
        0
    }
}

/// Lists the policy's defined roles plus built-in `owner` ordered from narrowest to broadest, each
/// paired with a plain one-line description.
pub fn role_choices(policy: &Policy) -> Vec<(String, String)> {
    let mut items: Vec<(u8, String, String)> = policy
        .roles
        .iter()
        .map(|(name, role)| {
            (
                role_rank(name, role),
                name.clone(),
                describe_role(name, role),
            )
        })
        .collect();
    items.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    let mut out: Vec<(String, String)> = items
        .into_iter()
        .map(|(_, name, desc)| (name, desc))
        .collect();
    out.push((OWNER.to_owned(), describe_role(OWNER, &Role::default())));
    out
}

/// Interactively asks for any missing device name and role without preset defaults.
pub fn prompt_pair_choices(
    policy: &Policy,
    existing_name: Option<&str>,
    existing_role: Option<&str>,
    allow_owner: bool,
    reader: &mut impl BufRead,
    writer: &mut impl Write,
) -> Result<(String, String, bool), ErrorBody> {
    let device_name = match existing_name {
        Some(name) => name.trim().to_owned(),
        None => {
            write!(writer, "What should this device be called? ").map_err(prompt_io)?;
            writer.flush().map_err(prompt_io)?;
            let mut line = String::new();
            let read = reader.read_line(&mut line).map_err(prompt_io)?;
            if read == 0 {
                return Err(ErrorBody::new(
                    ErrorCode::Cancelled,
                    "Pairing cancelled before a device name was given.",
                ));
            }
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.chars().any(char::is_control) {
                return Err(ErrorBody::new(
                    ErrorCode::InvalidArgs,
                    "A device name is required.",
                )
                .with_hint("Run `legion pair` again and enter a short name such as `pixel` or `tablet`."));
            }
            trimmed.to_owned()
        }
    };

    let choices = role_choices(policy);
    let role = match existing_role {
        Some(role) => role.to_owned(),
        None => {
            writeln!(writer, "What may {device_name} do?").map_err(prompt_io)?;
            if policy.roles.is_empty() {
                writeln!(
                    writer,
                    "  (No custom roles are defined in policy.toml yet; run `legion init` to add starter roles.)"
                )
                .map_err(prompt_io)?;
            }
            let width = choices.iter().map(|(n, _)| n.len()).max().unwrap_or(0);
            for (idx, (name, desc)) in choices.iter().enumerate() {
                writeln!(writer, "  {}. {name:<width$}  — {desc}", idx + 1).map_err(prompt_io)?;
            }
            write!(writer, "Choose a role (number or name): ").map_err(prompt_io)?;
            writer.flush().map_err(prompt_io)?;
            let mut line = String::new();
            let read = reader.read_line(&mut line).map_err(prompt_io)?;
            if read == 0 {
                return Err(ErrorBody::new(
                    ErrorCode::Cancelled,
                    "Pairing cancelled before a role was chosen.",
                ));
            }
            let answer = line.trim();
            if answer.is_empty() {
                return Err(ErrorBody::new(
                    ErrorCode::InvalidArgs,
                    "A role must be chosen (no default role is preset).",
                )
                .with_hint("Enter a role number or name from the list above."));
            }
            if let Ok(num) = answer.parse::<usize>()
                && num >= 1
                && num <= choices.len()
            {
                choices[num - 1].0.clone()
            } else if let Some((name, _)) = choices.iter().find(|(n, _)| n == answer) {
                name.clone()
            } else {
                let names: Vec<&str> = choices.iter().map(|(n, _)| n.as_str()).collect();
                return Err(ErrorBody::new(
                    ErrorCode::InvalidArgs,
                    format!("{answer:?} is not one of the available roles."),
                )
                .with_hint(format!("Choose one of: {}.", names.join(", "))));
            }
        }
    };

    let confirmed_owner = if role == OWNER && !allow_owner {
        writeln!(
            writer,
            "Warning: `owner` lets {device_name} do everything this account can, including pairing further devices."
        )
        .map_err(prompt_io)?;
        write!(writer, "Grant full owner access to {device_name}? [y/N] ").map_err(prompt_io)?;
        writer.flush().map_err(prompt_io)?;
        let mut confirm = String::new();
        let read = reader.read_line(&mut confirm).map_err(prompt_io)?;
        if read == 0 {
            return Err(ErrorBody::new(
                ErrorCode::Cancelled,
                "Pairing with the owner role was cancelled.",
            ));
        }
        let reply = confirm.trim().to_ascii_lowercase();
        if reply == "y" || reply == "yes" {
            true
        } else {
            return Err(ErrorBody::new(
                ErrorCode::Cancelled,
                "Pairing with the owner role was cancelled.",
            )
            .with_hint(
                "Choose a narrower role from policy.toml, or answer `yes` / pass `--allow-owner`.",
            ));
        }
    } else {
        allow_owner
    };

    Ok((device_name, role, confirmed_owner))
}

fn prompt_io(error: io::Error) -> ErrorBody {
    ErrorBody::new(
        ErrorCode::Failed,
        format!("Could not read pairing choice from terminal: {error}."),
    )
}

/// Renders the human-readable output of `legion pair`:
/// two summary lines, the Unicode half-block QR code with quiet zone, the one-line invite text,
/// and the commands to list, update, and remove devices.
pub fn format_human(
    result: &Value,
    invert: bool,
    on_machine: Option<&str>,
) -> Result<String, ErrorBody> {
    let field = |name: &str| {
        result.get(name).and_then(Value::as_str).ok_or_else(|| {
            ErrorBody::new(
                ErrorCode::Internal,
                format!("device.invite reply is missing `{name}`."),
            )
        })
    };
    let invite = field("invite")?;
    let principal = field("principal")?;
    let role = field("role")?;
    let node = field("node")?;
    let expires = field("expires")?;
    let keyscan_only = result
        .get("host_keys_source")
        .and_then(Value::as_str)
        .is_some_and(|src| src == "ssh-keyscan only");

    let qr = render_qr(invite, invert)?;
    let on_flag = match on_machine {
        Some(machine) => format!(" --on {machine}"),
        None => String::new(),
    };

    let mut text = String::new();
    let _ = writeln!(
        text,
        "Device will act as {principal} (role: {role}) on {node}."
    );
    let _ = writeln!(text, "Invite valid until {expires} (single use).");
    if keyscan_only {
        text.push('\n');
        let _ = writeln!(
            text,
            "Warning: host keys came from ssh-keyscan only because default sshd host-key files could not be read."
        );
        let _ = writeln!(
            text,
            "         Set [devices] host_keys in node.toml to pin host-key files explicitly if another local process could listen on this port."
        );
    }
    text.push('\n');
    text.push_str(&qr);
    text.push_str("\n\n");
    text.push_str(invite);
    text.push_str("\n\n");
    let _ = writeln!(text, "List paired devices:  legion device.list{on_flag}");
    let _ = writeln!(
        text,
        "Change a role:        legion device.set name=<device> role=<role>{on_flag}"
    );
    let _ = writeln!(
        text,
        "Remove a device:      legion device.remove name=<device>{on_flag}"
    );
    if let Some(peers) = result.get("fleet_peers").and_then(Value::as_array) {
        let peer_names: Vec<&str> = peers.iter().filter_map(Value::as_str).collect();
        if !peer_names.is_empty() {
            let _ = writeln!(
                text,
                "Sync offline peers:   legion fleet.sync direction=push documents='[\"devices.toml\"]'{on_flag}  (fleet peers: {})",
                peer_names.join(", ")
            );
        }
    }
    Ok(text)
}

/// Renders `data` as a terminal QR code using Unicode half blocks (`▀`, `▄`, `█`, ` `) with a
/// 4-module quiet zone.
pub fn render_qr(data: &str, invert: bool) -> Result<String, ErrorBody> {
    let code =
        QrCode::with_error_correction_level(data.as_bytes(), EcLevel::L).map_err(|error| {
            ErrorBody::new(
                ErrorCode::Internal,
                format!("Cannot encode the invite as a QR code: {error}."),
            )
        })?;
    let (dark, light) = if invert {
        (Dense1x2::Dark, Dense1x2::Light)
    } else {
        (Dense1x2::Light, Dense1x2::Dark)
    };
    Ok(code
        .render::<Dense1x2>()
        .dark_color(dark)
        .light_color(light)
        .quiet_zone(true)
        .module_dimensions(1, 1)
        .build())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::io::Cursor;

    use serde_json::json;

    use super::*;

    fn starter_policy() -> Policy {
        parse::<Policy>(
            Path::new("policy.toml"),
            br#"
default = "owner"

[roles.observer]
allow = ["effect:read"]
deny = ["audit.*"]

[roles.editor]
allow = ["effect:read", "effect:write"]
deny = ["audit.*"]

[roles.operator]
allow = ["effect:read", "effect:write", "effect:exec", "effect:destroy"]
deny = ["effect:power", "effect:admin"]
"#,
        )
        .unwrap()
    }

    #[test]
    fn qr_renders_half_blocks_with_quiet_zone_and_invert_flips_polarity() {
        let normal = render_qr("legion-invite-1.eyJ2IjoxfQ", false).unwrap();
        let inverted = render_qr("legion-invite-1.eyJ2IjoxfQ", true).unwrap();

        assert_ne!(normal, inverted);
        assert!(normal.contains('▀') || normal.contains('▄'));
        assert!(normal.contains('█'));
        assert!(inverted.contains('▀') || inverted.contains('▄'));
        // Default polarity starts with a solid foreground quiet-zone line; inverted starts with spaces.
        assert!(normal.lines().next().unwrap().chars().all(|c| c == '█'));
        assert!(inverted.lines().next().unwrap().chars().all(|c| c == ' '));
    }

    #[test]
    fn prompt_lists_policy_roles_in_plain_words_and_has_no_preset_defaults() {
        let policy = starter_policy();

        // Choosing by number selects the expected role.
        let mut out = Vec::new();
        let mut input = Cursor::new(b"Pixel 10\n2\n".to_vec());
        let (name, role, allow_owner) =
            prompt_pair_choices(&policy, None, None, false, &mut input, &mut out).unwrap();
        assert_eq!(name, "Pixel 10");
        assert_eq!(role, "editor");
        assert!(!allow_owner);
        let prompt_text = String::from_utf8(out).unwrap();
        assert!(prompt_text.contains("look only"), "{prompt_text}");
        assert!(prompt_text.contains("change files"), "{prompt_text}");
        assert!(prompt_text.contains("operate"), "{prompt_text}");
        assert!(prompt_text.contains("everything"), "{prompt_text}");

        // Blank role is refused (nothing preset).
        let mut out = Vec::new();
        let mut blank_role = Cursor::new(b"tablet\n\n".to_vec());
        let err =
            prompt_pair_choices(&policy, None, None, false, &mut blank_role, &mut out).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgs);

        // Picking `owner` interactively requires explicit confirmation.
        let mut out = Vec::new();
        let mut decline = Cursor::new(b"laptop\nowner\nn\n".to_vec());
        let err =
            prompt_pair_choices(&policy, None, None, false, &mut decline, &mut out).unwrap_err();
        assert_eq!(err.code, ErrorCode::Cancelled);

        let mut out = Vec::new();
        let mut accept = Cursor::new(b"laptop\nowner\nyes\n".to_vec());
        let (name, role, allow_owner) =
            prompt_pair_choices(&policy, None, None, false, &mut accept, &mut out).unwrap();
        assert_eq!(name, "laptop");
        assert_eq!(role, "owner");
        assert!(allow_owner);
    }

    #[test]
    fn human_output_contains_summary_qr_invite_and_commands() {
        let output = format_human(
            &json!({
                "invite": "legion-invite-1.eyJ2IjoxfQ",
                "node": "tower",
                "principal": "device:pixel",
                "role": "observer",
                "expires": "2026-11-24T12:15:00Z"
            }),
            false,
            None,
        )
        .unwrap();
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(
            lines[0],
            "Device will act as device:pixel (role: observer) on tower."
        );
        assert_eq!(
            lines[1],
            "Invite valid until 2026-11-24T12:15:00Z (single use)."
        );
        assert!(output.contains("legion-invite-1.eyJ2IjoxfQ"));
        assert!(output.contains("legion device.list"));
        assert!(output.contains("legion device.set"));
        assert!(output.contains("legion device.remove"));
        assert!(!output.contains("Warning:"));

        // When host keys come from ssh-keyscan alone, human output emits a prominent warning.
        let warned = format_human(
            &json!({
                "invite": "legion-invite-1.eyJ2IjoxfQ",
                "node": "tower",
                "principal": "phone",
                "role": "observer",
                "expires": "2026-11-24T12:15:00Z",
                "host_keys_source": "ssh-keyscan only"
            }),
            false,
            None,
        )
        .unwrap();
        assert!(
            warned.contains("Warning:") && warned.contains("ssh-keyscan only"),
            "expected prominent ssh-keyscan only warning, got:\n{warned}"
        );
    }
}
