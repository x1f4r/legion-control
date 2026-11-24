//! Configuration files: strict TOML, read again whenever they change.
//!
//! A missing file is its document's default. A file that does not parse is an error naming the file
//! and the line, never a quiet fall back to the default: for `policy.toml` the default is "the owner
//! may do anything", and a typo must not grant that. For the same reason a file that is there but
//! says nothing is refused unless its document allows it, and every configuration file is written
//! with [`write_atomically`], so a reader never sees one half written.

use std::any::Any;
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use legion_proto::{ErrorBody, ErrorCode};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::json;

mod power;

pub use power::{BootTarget, BusyConfig, BusyProbe, DEFAULT_PROBE_S, LONGEST_PROBE_S, PowerConfig};

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

    /// Whether a file that exists but says nothing (it is empty, or holds only whitespace and
    /// comments) reads as the default. Off unless a document turns it on: a file being rewritten
    /// in place can read as empty for a moment, and where the default allows more than a written
    /// file would, as with `policy.toml`, that moment must not widen access. A document whose
    /// default grants nothing, such as `node.toml`, can turn it on.
    const EMPTY_IS_DEFAULT: bool = false;
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
    /// the file and the line when it does not parse or, unless the document allows it, says
    /// nothing.
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

    /// Replaces a document's file with `text`, creating the directory if need be.
    ///
    /// The text is parsed first and refused, with the line, if it would not be read back, so a
    /// tool that edits configuration can never leave a file that fails every later call. The file
    /// is replaced with [`write_atomically`]. It blocks, so async code calls it from
    /// `spawn_blocking`.
    pub fn write<D: Document>(&self, text: &str) -> Result<(), ErrorBody> {
        let path = self.path_of::<D>();
        parse::<D>(&path, text.as_bytes())?;
        std::fs::create_dir_all(&self.dir)
            .and_then(|()| write_atomically(&path, text.as_bytes()))
            .map_err(|error| {
                ErrorBody::new(
                    ErrorCode::Failed,
                    format!("{} cannot be written: {error}.", path.display()),
                )
                .with_hint(
                    "Make the configuration directory writable by the account legion runs as.",
                )
            })
    }
}

/// How long replacing a file keeps trying while something holds it open. On Windows an indexer or
/// a virus scanner that has the file open makes a rename fail for a moment; anywhere else a failed
/// rename has failed for good.
const REPLACE_PATIENCE: Duration = Duration::from_secs(3);

/// Writes a file so that a reader at the same moment sees the old file or the new one, never part of
/// either, and a crash leaves one of the two: the bytes go to a temporary file beside it, are
/// flushed to disk, and the temporary file is renamed over the old one, keeping its permissions.
///
/// Every configuration file the workspace writes goes through here. It blocks, retrying a rename
/// on Windows for a few seconds, so async code calls it from `spawn_blocking`.
pub fn write_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    };
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut temporary = tempfile::Builder::new()
        .prefix(&format!(".{name}."))
        .suffix(".tmp")
        .tempfile_in(dir)?;
    temporary.write_all(bytes)?;
    match std::fs::metadata(path) {
        Ok(old) => temporary.as_file().set_permissions(old.permissions())?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    temporary.as_file().sync_all()?;
    let started = Instant::now();
    let mut pause = Duration::from_millis(10);
    loop {
        match temporary.persist(path) {
            Ok(_) => return Ok(()),
            Err(error)
                if cfg!(windows)
                    && error.error.kind() == io::ErrorKind::PermissionDenied
                    && started.elapsed() < REPLACE_PATIENCE =>
            {
                temporary = error.file;
                std::thread::sleep(pause);
                pause = (pause * 2).min(Duration::from_millis(250));
            }
            // Dropping the temporary file removes it.
            Err(error) => return Err(error.error),
        }
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
    let says_nothing = !D::EMPTY_IS_DEFAULT
        && toml::from_str::<toml::Table>(text).is_ok_and(|table| table.is_empty());
    if says_nothing {
        let file = path.file_name().map_or_else(
            || path.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        );
        return Err(ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "{} is there but says nothing, and it is refused rather than read as the defaults.",
                path.display()
            ),
        )
        .with_hint(format!(
            "Delete {file} to get its defaults, or write what it should say into it; a file caught half way through being rewritten reads like this, so write it whole and rename it into place."
        ))
        .with_details(json!({"file": path})));
    }
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

/// `node.toml`: what this node calls itself and node-wide settings, including what keeps the
/// machine busy and how it sleeps and restarts (see [`BusyConfig`], [`PowerConfig`] and
/// [`BootTarget`]).
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeFile {
    /// The node's name; the host name when absent.
    #[serde(default)]
    pub name: Option<NodeName>,
    /// Settings for the `svc` tool family.
    #[serde(default)]
    pub svc: SvcConfig,
    /// Settings for `legion serve`.
    #[serde(default)]
    pub serve: ServeConfig,
    /// Which system of this machine this is, such as `linux` or `windows`. `hello` reports it, so
    /// a machine that was restarted into another system says which one answered.
    #[serde(default)]
    pub system: Option<NodeName>,
    /// What keeps this machine busy; absent leaves it unmonitored, which blocks unattended power
    /// changes.
    #[serde(default)]
    pub busy: Option<BusyConfig>,
    /// `[power]`: how this machine sleeps and whether system actions use `sudo`.
    #[serde(default)]
    pub power: PowerConfig,
    /// `[boot.<name>]`: the systems this machine can restart into, by name.
    #[serde(default)]
    pub boot: BTreeMap<String, BootTarget>,
}

/// The `[serve]` table of `node.toml`: what `legion serve` accepts besides this machine itself.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServeConfig {
    /// Browser origins (`https://app.example.net`) whose pages may call the server. A request
    /// from any other page is refused, so a site open in a browser on this machine cannot use it.
    #[serde(default)]
    pub origins: Vec<String>,
    /// Host names the server answers to besides loopback, such as the name Tailscale Serve
    /// publishes it under (`tower.example.ts.net`, or `tower.example.ts.net:8443` for one port).
    /// Any other `Host` is refused, which is what stops a DNS rebinding page from reaching it.
    #[serde(default)]
    pub hosts: Vec<String>,
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
    // Its default is the host's name, which grants nothing.
    const EMPTY_IS_DEFAULT: bool = true;

    fn validate(&self) -> Result<(), String> {
        power::validate(self.busy.as_ref(), &self.power, &self.boot)
    }
}

impl NodeFile {
    /// The name the file gives, or the one the host name makes.
    pub fn name_or_host(&self) -> NodeName {
        self.name.clone().unwrap_or_else(NodeName::from_host)
    }
}

/// The longest a node's name may be: what a DNS label may be, so a name can also be a host's.
const LONGEST_NAME: usize = 63;

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
    /// hold turned into `-`, and cut to the length a name may have.
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
        let separator = |c| c == '-' || c == '_';
        let trimmed = cleaned.trim_matches(separator);
        // Every character is ASCII by now, so any byte is a place to cut.
        let trimmed = trimmed
            .get(..LONGEST_NAME)
            .unwrap_or(trimmed)
            .trim_end_matches(separator);
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
        } else if !starts_well || !rest_fits || name.len() > LONGEST_NAME {
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
    fn a_file_that_says_nothing_is_refused_unless_its_document_allows_it() {
        let (dir, store) = store();
        for nothing in ["", "  \n\r\n", "# colour = \"red\"\n"] {
            std::fs::write(dir.path().join("sample.toml"), nothing).unwrap();
            let error = store.get::<Sample>().unwrap_err();
            assert_eq!(error.code, ErrorCode::Failed);
            assert!(error.message.contains("sample.toml"), "{}", error.message);
            assert!(
                error
                    .hint
                    .as_deref()
                    .is_some_and(|hint| hint.starts_with("Delete sample.toml")),
                "{error:?}"
            );
            std::fs::write(dir.path().join("node.toml"), nothing).unwrap();
            assert!(store.get::<NodeFile>().unwrap().name.is_none());
        }
    }

    #[test]
    fn writing_atomically_replaces_the_whole_file_and_leaves_nothing_beside_it() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("policy.toml");
        write_atomically(&file, b"first").unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"first");
        write_atomically(&file, b"second, longer").unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"second, longer");
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["policy.toml"]);
        assert!(write_atomically(&dir.path().join("no").join("such.toml"), b"x").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn writing_atomically_keeps_the_permissions_of_the_file_it_replaces() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("fleet.toml");
        std::fs::write(&file, "old").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o640)).unwrap();
        write_atomically(&file, b"new").unwrap();
        let mode = std::fs::metadata(&file).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o640);
    }

    #[test]
    fn a_store_writes_only_what_it_would_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let store = ConfigStore::new(dir.path().join("config"));
        let error = store.write::<Sample>("size = 11\n").unwrap_err();
        assert!(
            error.message.contains("size is at most 10"),
            "{}",
            error.message
        );
        assert!(store.write::<Sample>("").is_err());
        assert!(!store.path_of::<Sample>().exists());
        store.write::<Sample>("colour = \"red\"\n").unwrap();
        assert_eq!(store.get::<Sample>().unwrap().colour, "red");
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
    fn a_long_host_name_makes_a_name_the_file_would_accept() {
        let long = NodeName::from_host_name(&"x".repeat(80));
        assert_eq!(long.as_str(), "x".repeat(63));
        assert!(NodeName::try_from(long.as_str().to_owned()).is_ok());
        // Cut where it ends in a separator, the separator goes too.
        let cut = format!("{}-{}", "a".repeat(62), "b".repeat(17));
        assert_eq!(NodeName::from_host_name(&cut).as_str(), "a".repeat(62));
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

    fn node(text: &str) -> Result<NodeFile, ErrorBody> {
        parse::<NodeFile>(Path::new("node.toml"), text.as_bytes())
    }

    #[test]
    fn node_toml_reads_the_power_settings_it_documents() {
        let file = node(
            r#"
system = "linux"

[[busy.probes]]
name = "render"
run = ["pgrep", "-x", "blender"]

[[busy.probes]]
name = "backup"
run = ["backup-running"]
timeout_s = 20

[power]
sudo = true
sleep = ["systemctl", "hibernate"]
before_sleep = [["stop-keepawake"], ["sync"]]

[boot.windows]
efi_label = "Windows Boot Manager"

[boot.other]
arm = ["sudo", "-n", "grub-reboot", "Other"]
verify = ["check-next-entry", "Other"]
"#,
        )
        .unwrap();
        assert_eq!(file.system.as_ref().map(NodeName::as_str), Some("linux"));
        let Some(BusyConfig::Probes(probes)) = &file.busy else {
            panic!("{:?}", file.busy)
        };
        assert_eq!(probes[0].name, "render");
        assert_eq!(probes[0].timeout_s, DEFAULT_PROBE_S);
        assert_eq!(probes[1].timeout_s, 20);
        assert!(file.power.sudo);
        assert_eq!(
            file.power.sleep.as_deref(),
            Some(&["systemctl".to_owned(), "hibernate".to_owned()][..])
        );
        assert_eq!(file.power.before_sleep.len(), 2);
        assert_eq!(
            file.boot.get("windows"),
            Some(&BootTarget::EfiLabel("Windows Boot Manager".to_owned()))
        );
        assert!(matches!(
            file.boot.get("other"),
            Some(BootTarget::Commands { .. })
        ));

        let never = node("busy = \"none\"\n").unwrap();
        assert_eq!(never.busy, Some(BusyConfig::Never));
        let unmonitored = node("").unwrap();
        assert_eq!(unmonitored.busy, None);
        assert!(unmonitored.boot.is_empty());
        assert!(!unmonitored.power.sudo);
    }

    #[test]
    fn node_toml_refuses_power_settings_that_would_mislead() {
        let cases: &[(&str, &str)] = &[
            ("busy = \"idle\"\n", "busy = \"none\""),
            ("busy = true\n", "busy"),
            ("[busy]\nprobes = []\n", "busy.probes is empty"),
            ("[busy]\nprobe = []\n", "probe"),
            (
                "[[busy.probes]]\nname = \"a\"\nrun = []\n",
                "busy.probes[0].run",
            ),
            (
                "[[busy.probes]]\nname = \"a\"\nrun = [\"x\"]\ntimeout_s = 0\n",
                "timeout_s",
            ),
            (
                "[[busy.probes]]\nname = \"a\"\nrun = [\"x\"]\ntimeout_s = 61\n",
                "timeout_s",
            ),
            (
                "[[busy.probes]]\nname = \"a\"\nrun = [\"x\"]\n[[busy.probes]]\nname = \"a\"\nrun = [\"y\"]\n",
                "already called",
            ),
            (
                "[[busy.probes]]\nname = \"a\"\nrun = [\"x\"]\ncommand = [\"y\"]\n",
                "command",
            ),
            ("[power]\nsleep = []\n", "power.sleep"),
            (
                "[power]\nbefore_sleep = [[\"\"]]\n",
                "power.before_sleep[0]",
            ),
            ("[power]\nsudo = \"yes\"\n", "line 2"),
            ("[boot.windows]\narm = [\"x\"]\n", "verify"),
            ("[boot.windows]\nverify = [\"x\"]\n", "arm"),
            (
                "[boot.windows]\nefi_label = \"W\"\narm = [\"x\"]\nverify = [\"y\"]\n",
                "not both",
            ),
            ("[boot.windows]\n", "efi_label"),
            ("[boot.windows]\nefi_label = \" \"\n", "empty"),
            ("[boot.Windows]\nefi_label = \"W\"\n", "boot.Windows"),
            ("[boot.windows]\nefi_lable = \"W\"\n", "efi_lable"),
            ("system = \"Linux\"\n", "line 1"),
        ];
        for (text, expected) in cases {
            let error = node(text).unwrap_err();
            assert_eq!(error.code, ErrorCode::Failed, "{text}");
            assert!(
                error.message.contains(expected),
                "{text:?}: {}",
                error.message
            );
        }
    }
}
