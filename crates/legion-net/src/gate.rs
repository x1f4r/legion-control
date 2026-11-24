//! What `legion gate` lets through: the command a client asked sshd for, when it is one of the
//! fixed commands this crate's ssh connector sends, and nothing else.
//!
//! An `authorized_keys` line that forces `legion gate --as <principal>` makes a key a wall: sshd
//! runs the gate whatever the client asked for and hands it the request in
//! `SSH_ORIGINAL_COMMAND`. The gate never runs that string. It only recognises it, by building
//! every command [`remote_command`] can produce and comparing them exactly, so the connector and
//! the gate cannot drift apart, and serves the session itself, pinned to the principal.

use crate::config::RemoteOs;
use crate::connector::{Subcommand, remote_command};

/// The subcommand `original` asks for, when it is exactly a command the ssh connector sends.
pub fn requested(original: &str) -> Option<Subcommand> {
    let mut programs: Vec<Option<&str>> = vec![None];
    programs.extend(program_candidates(original).into_iter().map(Some));
    for program in programs {
        for os in [RemoteOs::Unix, RemoteOs::Windows] {
            for subcommand in [Subcommand::Rpc, Subcommand::Stream] {
                if remote_command(os, subcommand, program).is_ok_and(|sent| sent == original) {
                    return Some(subcommand);
                }
            }
        }
    }
    None
}

/// The programs a connector with a `program` setting could have named in `original`. Only a
/// guess at where the name sits: whatever it finds must still rebuild `original` exactly.
fn program_candidates(original: &str) -> Vec<&str> {
    let mut found = Vec::new();
    if let Some((program, _)) = original.rsplit_once(' ') {
        found.push(program);
    }
    if let Some(rest) = original
        .strip_prefix("sh -c 'exec ")
        .and_then(|rest| rest.strip_suffix('\''))
        && let Some((program, _)) = rest.rsplit_once(' ')
    {
        found.push(program);
    }
    if let Some(rest) = original.strip_prefix("sh -c 'command -v ")
        && let Some((program, _)) = rest.split_once(' ')
    {
        found.push(program);
    }
    found
}

/// What the gate says to a client that asked for anything else.
pub const REFUSAL: &str = "legion gate: this key may only run `legion rpc` or `legion stream`, exactly as legion's ssh connector sends them; nothing else is run.";

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    const PROGRAMS: [Option<&str>; 6] = [
        None,
        Some("legion"),
        Some("legion-beta"),
        Some("/opt/legion/legion"),
        Some("~/tools/legion"),
        Some(r"C:\Tools\legion.exe"),
    ];

    #[test]
    fn every_command_the_connector_sends_is_accepted() {
        let mut seen = 0;
        for program in PROGRAMS {
            for os in [RemoteOs::Unix, RemoteOs::Windows] {
                for subcommand in [Subcommand::Rpc, Subcommand::Stream] {
                    let sent = remote_command(os, subcommand, program).unwrap();
                    assert_eq!(requested(&sent), Some(subcommand), "{sent}");
                    seen += 1;
                }
            }
        }
        assert_eq!(seen, 24);
    }

    #[test]
    fn near_misses_are_refused() {
        let unix = remote_command(RemoteOs::Unix, Subcommand::Rpc, None).unwrap();
        let unix_path = remote_command(
            RemoteOs::Unix,
            Subcommand::Stream,
            Some("/opt/legion/legion"),
        )
        .unwrap();
        let mut misses = vec![
            String::new(),
            "legion".to_owned(),
            "legion rpc ".to_owned(),
            " legion rpc".to_owned(),
            "legion  rpc".to_owned(),
            "legion rpc\n".to_owned(),
            "legion rpc; id".to_owned(),
            "legion rpc --as owner".to_owned(),
            "legion rpcx".to_owned(),
            "legion Rpc".to_owned(),
            "legion RPC".to_owned(),
            "legion mcp".to_owned(),
            "legion gate --as owner".to_owned(),
            "legion fs.read path=/etc/shadow".to_owned(),
            "legion;id rpc".to_owned(),
            "$(id) rpc".to_owned(),
            "/path with space/legion rpc".to_owned(),
            "sh -c 'exec /opt/legion/legion rpc'x".to_owned(),
            "sh -c 'exec /opt/legion/legion rpc".to_owned(),
            "sh -c 'exec legion rpc; id'".to_owned(),
            "bash -c 'exec /opt/legion/legion rpc'".to_owned(),
            "sh".to_owned(),
            "scp -t /home/me".to_owned(),
            "internal-sftp".to_owned(),
        ];
        misses.push(format!("{unix} "));
        misses.push(format!("{unix}; id"));
        misses.push(unix.replace("rpc", "mcp"));
        misses.push(unix.replacen("legion", "legion2", 1));
        misses.push(unix_path.replace("stream", "streams"));
        misses.push(unix.to_uppercase());
        for miss in &misses {
            assert_eq!(requested(miss), None, "{miss:?} was accepted");
        }
    }
}
