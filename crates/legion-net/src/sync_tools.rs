//! Fleet bridge connecting `legion_tools::sync` fleet-coordinating tools (`sync.create`,
//! `sync.add`, `sync.remove`, `sync.delete`) to [`Fleet`].

use std::collections::BTreeMap;
use std::time::Duration;

use legion_core::{BoxFuture, Ctx, EventSink};
use legion_proto::ErrorBody;
use legion_tools::sync::SyncRemote;
use legion_tools::sync::config::load_sync_file;
use serde_json::{Value, json};

use crate::config::FleetFile;
use crate::pool::Fleet;

/// Per-peer timeout when best-effort pushing `sync.toml` after a `sync.*` mutation tool.
const SYNC_TOML_PUSH_TIMEOUT: Duration = Duration::from_secs(10);

/// [`SyncRemote`] implementation backed by [`Fleet`].
pub struct FleetSyncRemote {
    fleet: Fleet,
}

impl FleetSyncRemote {
    /// Creates a new fleet bridge for `sync.*` tools.
    pub fn new(fleet: Fleet) -> Self {
        Self { fleet }
    }
}

impl SyncRemote for FleetSyncRemote {
    fn call_remote<'a>(
        &'a self,
        ctx: &'a Ctx,
        machine: &'a str,
        tool: &'a str,
        args: Value,
    ) -> BoxFuture<'a, Result<Value, ErrorBody>> {
        Box::pin(async move {
            self.fleet
                .call_remote(ctx, machine, tool, args, EventSink::discard())
                .await
        })
    }

    fn push_sync_toml<'a>(
        &'a self,
        ctx: &'a Ctx,
        machines: Vec<String>,
    ) -> BoxFuture<'a, BTreeMap<String, Result<(), String>>> {
        Box::pin(async move {
            let here = ctx.node_name().to_owned();
            let fleet_file = match ctx.config().get::<FleetFile>() {
                Ok(f) => f,
                Err(_) => return BTreeMap::new(),
            };
            let targets: Vec<String> = if machines.is_empty() {
                fleet_file
                    .machines
                    .keys()
                    .filter(|m| m.as_str() != here)
                    .cloned()
                    .collect()
            } else {
                machines
                    .into_iter()
                    .filter(|m| m != &here && fleet_file.machine(m).is_some())
                    .collect()
            };
            if targets.is_empty() {
                return BTreeMap::new();
            }

            let paths = ctx.paths().clone();
            let content = match tokio::task::spawn_blocking(move || {
                let file = load_sync_file(&paths)?;
                file.to_canonical_toml()
            })
            .await
            {
                Ok(Ok(c)) => c,
                _ => return BTreeMap::new(),
            };

            let mut results = BTreeMap::new();
            for peer in targets {
                let fut = self.fleet.call_remote_as_node(
                    ctx,
                    &peer,
                    "config.write",
                    json!({
                        "document": "sync.toml",
                        "content": content,
                    }),
                    EventSink::discard(),
                );
                let outcome = match tokio::time::timeout(SYNC_TOML_PUSH_TIMEOUT, fut).await {
                    Ok(Ok(_)) => Ok(()),
                    Ok(Err(err)) => Err(err.message),
                    Err(_) => Err(format!(
                        "timed out after {}s",
                        SYNC_TOML_PUSH_TIMEOUT.as_secs()
                    )),
                };
                results.insert(peer, outcome);
            }
            results
        })
    }

    fn fleet_has_machine(&self, ctx: &Ctx, machine: &str) -> Result<bool, ErrorBody> {
        let fleet_file = ctx.config().get::<FleetFile>()?;
        Ok(fleet_file.machine(machine).is_some())
    }
}
