//! Small atomic cache under the state directory remembering which system of a multi-system
//! machine answered last and which remote SSH OS shape worked last.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use legion_os::atomic::{WriteOptions, write_atomic};
use serde::{Deserialize, Serialize};

use crate::config::RemoteOs;

const CACHE_FILE: &str = "fleet-cache.json";

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct CacheData {
    #[serde(default)]
    last_system: BTreeMap<String, String>,
    #[serde(default)]
    last_os: BTreeMap<String, RemoteOs>,
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
        let _ = write_atomic(&self.path, &bytes, WriteOptions::default());
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
    }
}
