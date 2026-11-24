//! Configuration files: strict TOML, read again whenever they change.
//!
//! A missing file is its document's default. A file that does not parse is an error naming the file
//! and the line, never a quiet fall back to the default: for `policy.toml` the default is "the owner
//! may do anything", and a typo must not grant that.

use std::any::Any;
use std::collections::HashMap;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use legion_proto::{ErrorBody, ErrorCode};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::json;

/// A configuration file with a fixed name in the configuration directory.
///
/// Every document refuses keys it does not know (`#[serde(deny_unknown_fields)]`), so a misspelt
/// key is an error rather than a setting that silently does nothing. A later milestone adds a
/// document by defining its type, implementing this trait, and registering it with
/// [`Registry::add_document`](crate::Registry::add_document) so `meta.doctor` checks it.
pub trait Document: DeserializeOwned + Default + Send + Sync + 'static {
    /// The file's name, such as `policy.toml`.
    const FILE: &'static str;

    /// Checks what the types alone cannot, such as one table naming a key of another. The message
    /// names the key; the file's name is added by the caller.
    fn validate(&self) -> Result<(), String> {
        Ok(())
    }
}

/// Reads documents from one directory and keeps each until its file's bytes change.
///
/// Comparing bytes rather than modification times costs one small read per call and cannot miss
/// an edit made within the same second, or one that kept the size.
pub struct ConfigStore {
    dir: PathBuf,
    cache: Mutex<HashMap<&'static str, Cached>>,
}

struct Cached {
    bytes: Option<Vec<u8>>,
    value: Arc<dyn Any + Send + Sync>,
}

impl ConfigStore {
    /// A store over the files in `dir`, which may not exist.
    pub fn new(dir: impl Into<PathBuf>) -> ConfigStore {
        ConfigStore {
            dir: dir.into(),
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// The directory the files are read from.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Where a document's file is.
    pub fn path_of<D: Document>(&self) -> PathBuf {
        self.dir.join(D::FILE)
    }

    /// The document as its file says now: the default when the file is missing, an error naming
    /// the file and the line when it does not parse.
    pub fn get<D: Document>(&self) -> Result<Arc<D>, ErrorBody> {
        let path = self.path_of::<D>();
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(ErrorBody::new(
                    ErrorCode::Failed,
                    format!("{} cannot be read: {error}.", path.display()),
                )
                .with_hint("Make the file readable by the account legion runs as."));
            }
        };

        // A poisoned lock means another call panicked while holding it; the cache is still a
        // plain map, so carry on with it rather than spreading the panic.
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(cached) = cache.get(D::FILE)
            && cached.bytes == bytes
            && let Ok(value) = Arc::clone(&cached.value).downcast::<D>()
        {
            return Ok(value);
        }

        let value = Arc::new(match &bytes {
            None => D::default(),
            Some(bytes) => parse::<D>(&path, bytes)?,
        });
        cache.insert(
            D::FILE,
            Cached {
                bytes,
                value: Arc::clone(&value) as Arc<dyn Any + Send + Sync>,
            },
        );
        Ok(value)
    }
}

impl fmt::Debug for ConfigStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConfigStore")
            .field("dir", &self.dir)
            .finish_non_exhaustive()
    }
}

/// Parses a document's text, naming `path` and the line in any error.
pub fn parse<D: Document>(path: &Path, bytes: &[u8]) -> Result<D, ErrorBody> {
    let fix_hint = "Fix the file. Until it parses, every call that needs it fails rather than using a default.";
    let text = std::str::from_utf8(bytes).map_err(|error| {
        let line = line_and_column(bytes, error.valid_up_to()).0;
        ErrorBody::new(
            ErrorCode::Failed,
            format!("{} is not UTF-8 text (line {line}).", path.display()),
        )
        .with_hint(fix_hint)
        .with_details(json!({"file": path, "line": line}))
    })?;
    let document: D = toml::from_str(text).map_err(|error| {
        let message = error.message().trim_end_matches('.').to_owned();
        match error.span() {
            Some(span) => {
                let (line, column) = line_and_column(bytes, span.start);
                ErrorBody::new(
                    ErrorCode::Failed,
                    format!(
                        "{} does not parse at line {line}: {message}.",
                        path.display()
                    ),
                )
                .with_hint(fix_hint)
                .with_details(json!({"file": path, "line": line, "column": column}))
            }
            None => ErrorBody::new(
                ErrorCode::Failed,
                format!("{} does not parse: {message}.", path.display()),
            )
            .with_hint(fix_hint)
            .with_details(json!({"file": path})),
        }
    })?;
    document.validate().map_err(|message| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("{}: {}.", path.display(), message.trim_end_matches('.')),
        )
        .with_hint(fix_hint)
        .with_details(json!({"file": path}))
    })?;
    Ok(document)
}

/// The 1-based line and column of a byte offset.
fn line_and_column(bytes: &[u8], offset: usize) -> (usize, usize) {
    let before = bytes.get(..offset).unwrap_or(bytes);
    let line = before.iter().filter(|byte| **byte == b'\n').count() + 1;
    let line_start = before
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |at| at + 1);
    let column = String::from_utf8_lossy(&before[line_start..])
        .chars()
        .count()
        + 1;
    (line, column)
}

/// `node.toml`: what this node calls itself and node-wide settings. Later milestones add its boot
/// targets and busy probes.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeFile {
    /// The node's name; the host name when absent.
    #[serde(default)]
    pub name: Option<NodeName>,
    /// Settings for the `svc` tool family.
    #[serde(default)]
    pub svc: SvcConfig,
}

/// The `[svc]` table of `node.toml`: how the node controls system services.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SvcConfig {
    /// Whether system-scope `svc.start`, `svc.stop` and `svc.restart` on Unix run through
    /// `sudo -n`. Defaults to `false` so the node never escalates privileges on its own; enabling
    /// it lets any principal allowed those tools control system services as root via
    /// non-interactive sudoers rules.
    #[serde(default)]
    pub sudo: bool,
}

impl Document for NodeFile {
    const FILE: &'static str = "node.toml";
}

impl NodeFile {
    /// The name the file gives, or the one the host name makes.
    pub fn name_or_host(&self) -> NodeName {
        self.name.clone().unwrap_or_else(NodeName::from_host)
    }
}

/// A node's name: lower-case letters, digits, `-` and `_`, starting with a letter or digit. Not
/// `all`, which an `on` uses to mean every machine.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Deserialize)]
#[serde(try_from = "String")]
pub struct NodeName(String);

impl NodeName {
    /// The name as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The name the host name gives: lower-cased, without its domain, anything else a name cannot
    /// hold turned into `-`.
    pub fn from_host() -> NodeName {
        NodeName::from_host_name(&gethostname::gethostname().to_string_lossy())
    }

    /// [`NodeName::from_host`] for a given host name.
    pub fn from_host_name(host: &str) -> NodeName {
        let short = host.split('.').next().unwrap_or_default().to_lowercase();
        let cleaned: String = short
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        let trimmed = cleaned.trim_matches(|c| c == '-' || c == '_');
        match trimmed {
            "" | "all" => NodeName("localhost".to_owned()),
            name => NodeName(name.to_owned()),
        }
    }
}

impl fmt::Display for NodeName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for NodeName {
    type Error = String;

    fn try_from(name: String) -> Result<Self, Self::Error> {
        let starts_well = name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
        let rest_fits = name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
        if name == "all" {
            Err("`all` means every machine and cannot be a node's name".to_owned())
        } else if !starts_well || !rest_fits || name.len() > 63 {
            Err(format!(
                "{name:?} is not a node name: use lower-case letters, digits, - and _, starting with a letter or digit"
            ))
        } else {
            Ok(NodeName(name))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Default, PartialEq, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Sample {
        #[serde(default)]
        colour: String,
        #[serde(default)]
        size: u32,
    }

    impl Document for Sample {
        const FILE: &'static str = "sample.toml";

        fn validate(&self) -> Result<(), String> {
            if self.size > 10 {
                Err("size is at most 10".to_owned())
            } else {
                Ok(())
            }
        }
    }

    fn store() -> (tempfile::TempDir, ConfigStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = ConfigStore::new(dir.path());
        (dir, store)
    }

    #[test]
    fn a_missing_file_is_the_default_and_so_is_a_missing_directory() {
        let (_dir, store) = store();
        assert_eq!(*store.get::<Sample>().unwrap(), Sample::default());
        let nowhere = ConfigStore::new(std::env::temp_dir().join("legion-no-such-dir-at-all"));
        assert_eq!(*nowhere.get::<Sample>().unwrap(), Sample::default());
    }

    #[test]
    fn a_file_that_does_not_parse_names_itself_and_the_line() {
        let (dir, store) = store();
        std::fs::write(
            dir.path().join("sample.toml"),
            "colour = \"red\"\nsize = \"big\"\n",
        )
        .unwrap();
        let error = store.get::<Sample>().unwrap_err();
        assert_eq!(error.code, ErrorCode::Failed);
        assert!(error.message.contains("sample.toml"), "{}", error.message);
        assert!(error.message.contains("line 2"), "{}", error.message);
        assert_eq!(
            error.details.as_ref().and_then(|d| d.get("line")),
            Some(&json!(2))
        );
    }

    #[test]
    fn an_unknown_key_is_an_error_not_a_setting_that_does_nothing() {
        let (dir, store) = store();
        std::fs::write(dir.path().join("sample.toml"), "\ncolor = \"red\"\n").unwrap();
        let error = store.get::<Sample>().unwrap_err();
        assert!(error.message.contains("line 2"), "{}", error.message);
        assert!(error.message.contains("color"), "{}", error.message);
    }

    #[test]
    fn validation_failures_name_the_file() {
        let (dir, store) = store();
        std::fs::write(dir.path().join("sample.toml"), "size = 11\n").unwrap();
        let error = store.get::<Sample>().unwrap_err();
        assert!(error.message.contains("sample.toml"), "{}", error.message);
        assert!(
            error.message.contains("size is at most 10"),
            "{}",
            error.message
        );
    }

    #[test]
    fn an_edit_is_seen_on_the_next_read_even_at_the_same_size() {
        let (dir, store) = store();
        let file = dir.path().join("sample.toml");
        std::fs::write(&file, "colour = \"red\"\n").unwrap();
        assert_eq!(store.get::<Sample>().unwrap().colour, "red");
        std::fs::write(&file, "colour = \"tan\"\n").unwrap();
        assert_eq!(store.get::<Sample>().unwrap().colour, "tan");
        std::fs::remove_file(&file).unwrap();
        assert_eq!(store.get::<Sample>().unwrap().colour, "");
    }

    #[test]
    fn node_names_are_strict_in_the_file_and_forgiving_from_the_host() {
        assert!(NodeName::try_from("tower".to_owned()).is_ok());
        assert!(NodeName::try_from("old-box_2".to_owned()).is_ok());
        assert!(NodeName::try_from("Tower".to_owned()).is_err());
        assert!(NodeName::try_from("all".to_owned()).is_err());
        assert!(NodeName::try_from(String::new()).is_err());
        assert!(NodeName::try_from("-x".to_owned()).is_err());
        assert_eq!(
            NodeName::from_host_name("Tower.example.net").as_str(),
            "tower"
        );
        assert_eq!(
            NodeName::from_host_name("Me's MacBook").as_str(),
            "me-s-macbook"
        );
        assert_eq!(NodeName::from_host_name("").as_str(), "localhost");
        assert_eq!(NodeName::from_host_name("ALL").as_str(), "localhost");
    }

    #[test]
    fn node_toml_takes_a_name_or_falls_back_to_the_host() {
        let named = parse::<NodeFile>(Path::new("node.toml"), b"name = \"tower\"\n").unwrap();
        assert_eq!(named.name_or_host().as_str(), "tower");
        assert!(!named.svc.sudo);
        let error = parse::<NodeFile>(Path::new("node.toml"), b"name = \"Tower\"\n").unwrap_err();
        assert!(error.message.contains("line 1"), "{}", error.message);
        let empty = parse::<NodeFile>(Path::new("node.toml"), b"").unwrap();
        assert_eq!(empty.name_or_host(), NodeName::from_host());
        assert!(!empty.svc.sudo);
    }

    #[test]
    fn node_toml_parses_svc_sudo_strictly() {
        let enabled = parse::<NodeFile>(Path::new("node.toml"), b"[svc]\nsudo = true\n").unwrap();
        assert!(enabled.svc.sudo);
        let disabled = parse::<NodeFile>(Path::new("node.toml"), b"[svc]\n").unwrap();
        assert!(!disabled.svc.sudo);
        let wrong_type =
            parse::<NodeFile>(Path::new("node.toml"), b"[svc]\nsudo = \"yes\"\n").unwrap_err();
        assert!(
            wrong_type.message.contains("line 2"),
            "{}",
            wrong_type.message
        );
        let unknown =
            parse::<NodeFile>(Path::new("node.toml"), b"[svc]\nroot = true\n").unwrap_err();
        assert!(unknown.message.contains("line 2"), "{}", unknown.message);
        assert!(unknown.message.contains("root"), "{}", unknown.message);
    }
}
