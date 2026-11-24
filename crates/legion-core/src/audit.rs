//! The audit log: one JSON line per call in `state/audit.jsonl`, appended and never rewritten.
//!
//! Writers and readers take a kernel lock on `state/audit.lock` first, so lines from processes
//! calling at the same moment never interleave, and a lock left by a process that died is released
//! with it.
//!
//! Arguments are recorded with what must not be kept replaced by `{"redacted_bytes": <size>}`:
//!
//! - file contents, which a tool takes in a field named `content`, `contents`, `data`, `bytes`,
//!   `body` or `stdin`;
//! - values under a key that names a secret (`password`, `token`, `secret`, `api_key`, …);
//! - strings that look like one: a private key, a bearer token, a known token prefix, a JWT, or
//!   `password=…` inside a longer string;
//! - any string longer than 4 KiB, which is content whatever its field is called.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

use legion_proto::ErrorCode;
use serde::{Serialize, Serializer};
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::ctx::Door;
use crate::glob;
use crate::paths::Paths;

/// One call, as the log records it.
#[derive(Clone, Debug, Serialize)]
pub struct Entry {
    /// When the call finished, RFC 3339 in UTC to the millisecond.
    pub time: String,
    /// The principal it acted as; `null` when the call failed before that was known.
    pub principal: Option<String>,
    /// The door it came through.
    pub door: Door,
    /// The tool it called.
    pub tool: String,
    /// The machine it ran on.
    pub target: String,
    /// `ok`, or the error code it failed with.
    pub outcome: Outcome,
    /// How long it took, in seconds.
    pub duration: f64,
    /// Its arguments, redacted.
    pub args: Value,
    /// The error message, when it failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// How a call ended: written `ok` or as the error code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// It returned a result.
    Ok,
    /// It failed with this code.
    Error(ErrorCode),
}

impl Outcome {
    /// The outcome as the log spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::Error(code) => code.as_str(),
        }
    }
}

impl Serialize for Outcome {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// The current time as the log writes it.
pub fn now() -> String {
    jiff::Timestamp::now()
        .strftime("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
}

/// The audit log of one node.
#[derive(Clone, Debug)]
pub struct AuditLog {
    log: PathBuf,
    lock: PathBuf,
}

/// Which entries [`AuditLog::tail`] returns; every field that is set must match.
#[derive(Clone, Debug, Default)]
pub struct Filter {
    /// The principal, exactly.
    pub principal: Option<String>,
    /// The tool, as a glob such as `fs.*`.
    pub tool: Option<String>,
    /// `ok` or an error code.
    pub outcome: Option<String>,
}

impl Filter {
    fn matches(&self, entry: &Value) -> bool {
        let field = |name: &str| entry.get(name).and_then(Value::as_str);
        self.principal
            .as_deref()
            .is_none_or(|want| field("principal") == Some(want))
            && self
                .tool
                .as_deref()
                .is_none_or(|want| field("tool").is_some_and(|tool| glob::matches(want, tool)))
            && self
                .outcome
                .as_deref()
                .is_none_or(|want| field("outcome") == Some(want))
    }
}

/// The last entries of the log.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Tail {
    /// The entries, oldest first.
    pub entries: Vec<Value>,
    /// Whether older matching entries were left out.
    pub truncated: bool,
}

impl AuditLog {
    /// The audit log under a node's state directory.
    pub fn new(paths: &Paths) -> AuditLog {
        AuditLog {
            log: paths.audit_log(),
            lock: paths.audit_lock(),
        }
    }

    /// Where the log is.
    pub fn path(&self) -> &std::path::Path {
        &self.log
    }

    fn lock_file(&self) -> io::Result<File> {
        if let Some(dir) = self.lock.parent() {
            std::fs::create_dir_all(dir)?;
        }
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&self.lock)
    }

    /// Appends one entry as one line, under the lock.
    pub fn append(&self, entry: &Entry) -> io::Result<()> {
        let mut line = serde_json::to_vec(entry)?;
        line.push(b'\n');
        let lock = self.lock_file()?;
        lock.lock()?;
        let written = OpenOptions::new()
            .append(true)
            .create(true)
            .open(&self.log)
            .and_then(|mut log| log.write_all(&line));
        // Released explicitly: Windows frees a lock left at close only when it gets round to it.
        let unlocked = lock.unlock();
        written.and(unlocked)
    }

    /// The last `limit` entries that match `filter`, oldest first.
    ///
    /// The log is read backwards in blocks and only as far as needed, so a long log costs no more
    /// than its tail. Lines that are not JSON, such as one cut short by a crash, are skipped.
    ///
    /// A filter that matches little can still read the whole log, so `cancel` is checked before
    /// every block, and a cancelled read fails with [`io::ErrorKind::Interrupted`].
    pub fn tail(
        &self,
        limit: usize,
        filter: &Filter,
        cancel: &CancellationToken,
    ) -> io::Result<Tail> {
        const BLOCK: u64 = 64 * 1024;
        let lock = self.lock_file()?;
        lock.lock_shared()?;
        let mut file = match File::open(&self.log) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                lock.unlock()?;
                return Ok(Tail {
                    entries: Vec::new(),
                    truncated: false,
                });
            }
            Err(error) => {
                let _ = lock.unlock();
                return Err(error);
            }
        };

        let mut newest_first = Vec::new();
        let mut truncated = false;
        // The start of the earliest line read so far, which may continue in the block before.
        let mut partial: Vec<u8> = Vec::new();
        let mut end = file.metadata()?.len();
        'blocks: while end > 0 {
            if cancel.is_cancelled() {
                let _ = lock.unlock();
                return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
            }
            let start = end.saturating_sub(BLOCK);
            let mut block = vec![0; usize::try_from(end - start).unwrap_or(0)];
            file.seek(SeekFrom::Start(start))?;
            file.read_exact(&mut block)?;
            block.extend_from_slice(&partial);
            end = start;

            let complete = if start == 0 {
                partial = Vec::new();
                &block[..]
            } else {
                match block.iter().position(|byte| *byte == b'\n') {
                    Some(first_newline) => {
                        partial = block[..first_newline].to_vec();
                        &block[first_newline + 1..]
                    }
                    None => {
                        partial = block;
                        continue;
                    }
                }
            };
            for line in complete.rsplit(|byte| *byte == b'\n') {
                let line = line.strip_suffix(b"\r").unwrap_or(line);
                let Ok(entry) = serde_json::from_slice::<Value>(line) else {
                    continue;
                };
                if !filter.matches(&entry) {
                    continue;
                }
                if newest_first.len() == limit {
                    truncated = true;
                    break 'blocks;
                }
                newest_first.push(entry);
            }
        }
        lock.unlock()?;
        newest_first.reverse();
        Ok(Tail {
            entries: newest_first,
            truncated,
        })
    }
}

const CONTENT_KEYS: [&str; 6] = ["content", "contents", "data", "bytes", "body", "stdin"];
const SECRET_WORDS: [&str; 12] = [
    "password",
    "passwd",
    "passphrase",
    "secret",
    "token",
    "credential",
    "cookie",
    "authorization",
    "api_key",
    "apikey",
    "private_key",
    "access_key",
];
const SECRET_PREFIXES: [&str; 15] = [
    "-----BEGIN",
    "Bearer ",
    "Basic ",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "github_pat_",
    "glpat-",
    "sk-",
    "xoxb-",
    "xoxp-",
    "AKIA",
    "ASIA",
];
const LONGEST_KEPT: usize = 4096;

/// Arguments as the log records them.
pub fn redact(args: &Value) -> Value {
    redact_value(None, args)
}

fn redact_value(key: Option<&str>, value: &Value) -> Value {
    if key.is_some_and(is_sensitive_key) {
        return size_of(value);
    }
    match value {
        Value::String(text) if text.len() > LONGEST_KEPT || looks_secret(text) => size_of(value),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, value)| (key.clone(), redact_value(Some(key), value)))
                .collect::<Map<String, Value>>(),
        ),
        Value::Array(items) => {
            Value::Array(items.iter().map(|item| redact_value(None, item)).collect())
        }
        other => other.clone(),
    }
}

fn is_sensitive_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    CONTENT_KEYS.contains(&key.as_str()) || SECRET_WORDS.iter().any(|word| key.contains(word))
}

fn looks_secret(text: &str) -> bool {
    let trimmed = text.trim_start();
    if SECRET_PREFIXES
        .iter()
        .any(|prefix| trimmed.starts_with(prefix))
    {
        return true;
    }
    let is_jwt = trimmed.starts_with("eyJ")
        && trimmed.split('.').count() == 3
        && !trimmed.contains(char::is_whitespace);
    if is_jwt {
        return true;
    }
    // A secret passed inside a longer string, such as `--password=hunter2` in a command.
    let lower = text.to_ascii_lowercase();
    SECRET_WORDS.iter().any(|word| {
        lower
            .match_indices(word)
            .any(|(at, _)| matches!(lower.as_bytes().get(at + word.len()), Some(b'=' | b':')))
    })
}

/// What a redacted value is replaced by: its size, in bytes of UTF-8 for a string and of JSON for
/// anything else.
fn size_of(value: &Value) -> Value {
    let bytes = match value {
        Value::String(text) => text.len(),
        other => other.to_string().len(),
    };
    json!({"redacted_bytes": bytes})
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn entry(tool: &str, principal: &str, outcome: Outcome) -> Entry {
        Entry {
            time: now(),
            principal: Some(principal.to_owned()),
            door: Door::Cli,
            tool: tool.to_owned(),
            target: "tower".to_owned(),
            outcome,
            duration: 0.001,
            args: json!({}),
            message: None,
        }
    }

    #[test]
    fn time_is_rfc_3339_utc_to_the_millisecond() {
        let time = now();
        assert_eq!(time.len(), "2026-01-02T03:04:05.678Z".len(), "{time}");
        assert!(time.ends_with('Z'));
        assert_eq!(&time[10..11], "T");
    }

    #[test]
    fn contents_and_secrets_are_replaced_by_their_size() {
        let args = json!({
            "path": "/home/me/notes.txt",
            "content": "twelve bytes",
            "env": {"GITHUB_TOKEN": "x", "LANG": "C"},
            "argv": ["mysql", "--password=hunter2", "-h", "db"],
            "header": "Bearer abc",
            "jwt": "eyJhbGciOi.eyJzdWIiOi.c2lnbmF0dXJl",
            "count": 3,
            "nested": {"api_key": {"a": 1}},
            "huge": "x".repeat(5000),
        });
        let redacted = redact(&args);
        assert_eq!(redacted["path"], json!("/home/me/notes.txt"));
        assert_eq!(redacted["content"], json!({"redacted_bytes": 12}));
        assert_eq!(
            redacted["env"]["GITHUB_TOKEN"],
            json!({"redacted_bytes": 1})
        );
        assert_eq!(redacted["env"]["LANG"], json!("C"));
        assert_eq!(redacted["argv"][0], json!("mysql"));
        assert_eq!(redacted["argv"][1], json!({"redacted_bytes": 18}));
        assert_eq!(redacted["argv"][3], json!("db"));
        assert_eq!(redacted["header"], json!({"redacted_bytes": 10}));
        assert_eq!(redacted["jwt"]["redacted_bytes"], json!(34));
        assert_eq!(redacted["count"], json!(3));
        assert_eq!(redacted["nested"]["api_key"], json!({"redacted_bytes": 7}));
        assert_eq!(redacted["huge"], json!({"redacted_bytes": 5000}));
        assert_eq!(
            redact(&json!({"key": "build-7", "tokens": 3}))["key"],
            json!("build-7")
        );
    }

    #[test]
    fn appended_entries_come_back_newest_last_and_filtered() {
        let dir = tempfile::tempdir().unwrap();
        let log = AuditLog::new(&Paths::under(dir.path()));
        assert_eq!(
            log.tail(5, &Filter::default(), &CancellationToken::new())
                .unwrap()
                .entries,
            Vec::<Value>::new()
        );
        for n in 0..10 {
            let outcome = if n % 2 == 0 {
                Outcome::Ok
            } else {
                Outcome::Error(ErrorCode::Denied)
            };
            log.append(&entry(&format!("meta.t{n}"), "owner", outcome))
                .unwrap();
        }
        log.append(&entry("fs.read", "agent:claude", Outcome::Ok))
            .unwrap();

        let last = log
            .tail(3, &Filter::default(), &CancellationToken::new())
            .unwrap();
        let tools: Vec<&str> = last
            .entries
            .iter()
            .filter_map(|e| e["tool"].as_str())
            .collect();
        assert_eq!(tools, ["meta.t8", "meta.t9", "fs.read"]);
        assert!(last.truncated);

        let denied = Filter {
            outcome: Some("denied".to_owned()),
            ..Filter::default()
        };
        assert_eq!(
            log.tail(100, &denied, &CancellationToken::new())
                .unwrap()
                .entries
                .len(),
            5
        );
        let by_glob = Filter {
            tool: Some("fs.*".to_owned()),
            principal: Some("agent:claude".to_owned()),
            ..Filter::default()
        };
        let found = log.tail(100, &by_glob, &CancellationToken::new()).unwrap();
        assert_eq!(found.entries.len(), 1);
        assert!(!found.truncated);
    }

    #[test]
    fn tail_reads_across_blocks_and_skips_a_torn_line() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let log = AuditLog::new(&paths);
        let mut big = entry("meta.big", "owner", Outcome::Ok);
        big.args = json!({"note": "y".repeat(3000)});
        for _ in 0..60 {
            log.append(&big).unwrap();
        }
        std::fs::OpenOptions::new()
            .append(true)
            .open(paths.audit_log())
            .and_then(|mut file| file.write_all(b"{\"tool\": \"torn"))
            .unwrap();
        let tail = log
            .tail(50, &Filter::default(), &CancellationToken::new())
            .unwrap();
        assert_eq!(tail.entries.len(), 50);
        assert!(tail.truncated);
        assert!(tail.entries.iter().all(|e| e["tool"] == json!("meta.big")));
        assert_eq!(
            log.tail(1000, &Filter::default(), &CancellationToken::new())
                .unwrap()
                .entries
                .len(),
            60
        );
    }

    #[test]
    fn a_cancelled_tail_stops_before_reading_on() {
        let dir = tempfile::tempdir().unwrap();
        let log = AuditLog::new(&Paths::under(dir.path()));
        log.append(&entry("meta.version", "owner", Outcome::Ok))
            .unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let error = log.tail(10, &Filter::default(), &cancel).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        // The lock was given back: a writer can still append.
        log.append(&entry("meta.version", "owner", Outcome::Ok))
            .unwrap();
    }

    #[test]
    fn concurrent_writers_never_interleave_lines() {
        let dir = tempfile::tempdir().unwrap();
        let log = Arc::new(AuditLog::new(&Paths::under(dir.path())));
        let threads: Vec<_> = (0..8)
            .map(|n| {
                let log = Arc::clone(&log);
                std::thread::spawn(move || {
                    let mut line = entry("meta.write", &format!("agent:{n}"), Outcome::Ok);
                    line.args = json!({"note": "z".repeat(2000)});
                    for _ in 0..25 {
                        log.append(&line).unwrap();
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        let text = std::fs::read_to_string(log.path()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 200);
        assert!(
            lines
                .iter()
                .all(|line| serde_json::from_str::<Value>(line).is_ok())
        );
    }
}
