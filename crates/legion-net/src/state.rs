//! Small atomic cache under the state directory remembering which system of a multi-system
//! machine answered last and which remote SSH OS shape worked last.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use legion_core::config::write_atomically;
use serde::{Deserialize, Serialize};

use crate::config::RemoteOs;

const CACHE_FILE: &str = "fleet-cache.json";

/// Last node name, explicitness, and identity reported by a peer machine.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ReportedNode {
    pub(crate) node: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) explicit: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) node_id: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct CacheData {
    #[serde(default)]
    last_system: BTreeMap<String, String>,
    #[serde(default)]
    last_os: BTreeMap<String, RemoteOs>,
    #[serde(default)]
    last_node: BTreeMap<String, ReportedNode>,
}

/// Persisted optimization hints for faster reconnects across processes.
#[derive(Debug)]
pub(crate) struct StateStore {
    path: PathBuf,
    lock: Mutex<()>,
}

impl StateStore {
    pub(crate) fn new(state_dir: &Path) -> Self {
        Self {
            path: state_dir.join(CACHE_FILE),
            lock: Mutex::new(()),
        }
    }

    pub(crate) fn last_system(&self, machine: &str) -> Option<String> {
        let _guard = self
            .lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.load().last_system.get(machine).cloned()
    }

    pub(crate) fn last_os(&self, machine: &str, system: &str) -> Option<RemoteOs> {
        let _guard = self
            .lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let key = os_key(machine, system);
        self.load().last_os.get(&key).copied()
    }

    pub(crate) fn record_system(&self, machine: &str, system: &str) {
        let _guard = self
            .lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut data = self.load();
        if data.last_system.get(machine).map(String::as_str) == Some(system) {
            return;
        }
        data.last_system
            .insert(machine.to_owned(), system.to_owned());
        self.save(&data);
    }

    pub(crate) fn record_os(&self, machine: &str, system: &str, os: RemoteOs) {
        let _guard = self
            .lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let key = os_key(machine, system);
        let mut data = self.load();
        if data.last_os.get(&key) == Some(&os) {
            return;
        }
        data.last_os.insert(key, os);
        self.save(&data);
    }

    pub(crate) fn last_node(&self, machine: &str) -> Option<ReportedNode> {
        let _guard = self
            .lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.load().last_node.get(machine).cloned()
    }

    pub(crate) fn record_node(&self, machine: &str, entry: ReportedNode) {
        let _guard = self
            .lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut data = self.load();
        if data.last_node.get(machine) == Some(&entry) {
            return;
        }
        data.last_node.insert(machine.to_owned(), entry);
        self.save(&data);
    }

    pub(crate) fn clear_node(&self, machine: &str) {
        let _guard = self
            .lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut data = self.load();
        if data.last_node.remove(machine).is_some() {
            self.save(&data);
        }
    }

    fn load(&self) -> CacheData {
        std::fs::read(&self.path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    fn save(&self, data: &CacheData) {
        let Ok(bytes) = serde_json::to_vec_pretty(data) else {
            return;
        };
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = write_atomically(&self.path, &bytes);
    }
}

fn os_key(machine: &str, system: &str) -> String {
    format!("{machine}/{system}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_persists_last_system_and_last_os_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::new(dir.path());
        assert_eq!(store.last_system("tower"), None);
        assert_eq!(store.last_os("tower", "linux"), None);

        store.record_system("tower", "windows");
        store.record_os("tower", "windows", RemoteOs::Windows);

        // Fresh store against the same state dir sees the persisted values.
        let reloaded = StateStore::new(dir.path());
        assert_eq!(reloaded.last_system("tower").as_deref(), Some("windows"));
        assert_eq!(
            reloaded.last_os("tower", "windows"),
            Some(RemoteOs::Windows)
        );

        assert_eq!(reloaded.last_node("studio"), None);
        reloaded.record_node(
            "studio",
            ReportedNode {
                node: "someones-laptop".to_owned(),
                explicit: Some(false),
                node_id: Some("0123456789abcdef0123456789abcdef".to_owned()),
            },
        );
        let reloaded2 = StateStore::new(dir.path());
        assert_eq!(
            reloaded2.last_node("studio"),
            Some(ReportedNode {
                node: "someones-laptop".to_owned(),
                explicit: Some(false),
                node_id: Some("0123456789abcdef0123456789abcdef".to_owned()),
            })
        );
        reloaded2.clear_node("studio");
        assert_eq!(StateStore::new(dir.path()).last_node("studio"), None);
    }
}
