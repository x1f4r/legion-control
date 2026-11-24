//! Reaching other Legion nodes over `ssh` or custom pipe commands.
//!
//! This crate owns `fleet.toml`, the process connectors that open pipes to remote `legion rpc` and
//! `legion stream` processes, the multiplexed JSON-RPC 2.0 client session, the pooled
//! [`legion_core::Router`] implementation ([`Fleet`]), and the `fleet.*` tool family.
//!
//! Higher-level cross-machine tools (such as `fs.transfer`) depend on [`Fleet`] to reuse the
//! pooled JSON-RPC session ([`Fleet::session`], [`Fleet::call_remote`]) or open a dedicated raw
//! pipe for bulk data ([`Fleet::open_pipe`] with [`Subcommand::Stream`]).

pub mod config;
pub mod connector;
pub mod pool;
pub mod session;
mod state;
pub mod stream;
pub mod tools;

pub use config::{
    FleetFile, MachineEntry, RemoteOs, ResolvedSystem, SystemEntry, Transport, validate_mac,
    validate_machine_entry, validate_machine_name, validate_program, validate_ssh_target,
};
pub use connector::{Pipe, PipeHandle, StderrTail, Subcommand, remote_command, ssh_args};
pub use pool::{Fleet, MAX_HOPS, PingOutcome};
pub use session::{CallError, Session, SessionInfo};
pub use tools::{DiscoveredMachine, parse_tailscale_status, register};
