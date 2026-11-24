//! `legion pair`: terminal-friendly wrapper over `device.invite` with a Unicode half-block QR code.

use std::fmt::Write as _;
use std::process::ExitCode;

use legion_core::proto::{ErrorBody, ErrorCode, Target};
use legion_core::{Call, CancellationToken, Door, Node, RouteOutcome, Router};
use qrcode::render::unicode::Dense1x2;
use qrcode::{EcLevel, QrCode};
use serde_json::{Map, Value};

use crate::args::PairCommand;
use crate::{FAILED, err, out, print_json, render, run_call};

/// Calls `device.invite` and prints either raw JSON (`--json`) or human-readable pairing output
/// containing summary lines, a terminal QR code, the one-line invite text, and management commands.
pub async fn run(
    node: &Node,
    router: &dyn Router,
    command: PairCommand,
    shutdown: CancellationToken,
) -> ExitCode {
    let mut args = Map::new();
    args.insert("principal".to_owned(), Value::String(command.principal));
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
    if command.allow_owner {
        args.insert("allow_owner".to_owned(), Value::Bool(true));
    }

    let on_name = command.on.clone();
    let on = command.on.map(Target::One);
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

/// Renders the human-readable output of `legion pair`:
/// two summary lines, the Unicode half-block QR code with quiet zone, the one-line invite text,
/// and the two commands to list and remove devices.
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
        "Remove a device:      legion device.remove name=<device>{on_flag}"
    );
    Ok(text)
}

/// Renders `data` as a terminal QR code using Unicode half blocks (`▀`, `▄`, `█`, ` `) with a
/// 4-module quiet zone.
///
/// By default (`invert = false`), light modules (including the quiet zone) are drawn in the
/// terminal foreground (`█`) and dark modules in the background (` `), which renders as a true
/// black-on-white QR code on dark terminals and a framed inverted QR code on light terminals.
/// Setting `invert = true` swaps foreground and background so light terminals render dark modules
/// in foreground (`█`) on the light background (` `).
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

    use serde_json::json;

    use super::*;

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
    fn human_output_contains_summary_qr_invite_and_commands() {
        let output = format_human(
            &json!({
                "invite": "legion-invite-1.eyJ2IjoxfQ",
                "node": "tower",
                "principal": "phone",
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
            "Device will act as phone (role: observer) on tower."
        );
        assert_eq!(
            lines[1],
            "Invite valid until 2026-11-24T12:15:00Z (single use)."
        );
        assert!(output.contains("legion-invite-1.eyJ2IjoxfQ"));
        assert!(output.contains("legion device.list"));
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
