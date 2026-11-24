//! The audit log: one JSON line per call in `state/audit.jsonl`, appended and never rewritten.
//!
//! Writers and readers take a kernel lock on `state/audit.lock` first, so lines from processes
//! calling at the same moment never interleave, and a lock left by a process that died is released
//! with it.
//!
//! Arguments, and the message of a call that failed, which can echo them, are recorded with what
//! must not be kept replaced by `{"redacted_bytes": <size>}`. Each rule is a table below; when a
//! rule is unsure it redacts, since a size lost costs little and a secret kept costs a lot.
//!
//! - file contents, which a tool takes in a field named `content`, `contents`, `data`, `bytes`,
//!   `body`, `stdin`, `old` or `new`;
//! - values under a key that names a secret, however it is spelt (`password`, `X-API-Key`,
//!   `apiKey`, `signing_key`, `auth`, …);
//! - in a list of arguments, the value after a flag that takes a secret (`--password`, `-p`,
//!   `--token`, `-u`);
//! - strings that carry one anywhere: a private key, an `Authorization` scheme, a known token
//!   prefix, a JWT, credentials in a URL, `password = …` or `?token=…`, or a flag that takes a
//!   secret followed by its value;
//! - any string longer than 4 KiB, which is content whatever its field is called, and all of the
//!   arguments when together they pass 16 KiB.

use std::borrow::Cow;
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
    /// Its arguments, from [`redact`].
    pub args: Value,
    /// The error message, when it failed, from [`redact_message`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<Value>,
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
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Tail {
    /// The entries, oldest first.
    pub entries: Vec<Value>,
    /// Whether older matching entries were left out.
    pub truncated: bool,
    /// How many lines among those read were not entries: cut short by a crash, damaged, or longer
    /// than any entry the log writes.
    pub skipped: usize,
}

/// How much of the log is read at a time, backwards from its end.
const BLOCK: usize = 64 * 1024;

/// The longest line [`AuditLog::tail`] reads as an entry. One the log wrote stays well below it,
/// since its arguments are held to [`LARGEST_ARGS`] and its message to [`LONGEST_KEPT`]; a longer
/// line is damage, and is skipped without being gathered into memory.
const LONGEST_ENTRY: usize = 64 * 1024;

/// The audit lock, held until it is dropped, so every way out of a function gives it back.
/// Released explicitly rather than left to the file closing: Windows frees a lock left at close
/// only when it gets round to it.
struct Held(File);

impl Drop for Held {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
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

    fn hold(&self, shared: bool) -> io::Result<Held> {
        let file = self.lock_file()?;
        if shared {
            file.lock_shared()?;
        } else {
            file.lock()?;
        }
        Ok(Held(file))
    }

    /// Appends one entry as one line, under the lock.
    pub fn append(&self, entry: &Entry) -> io::Result<()> {
        let mut line = serde_json::to_vec(entry)?;
        line.push(b'\n');
        let _held = self.hold(false)?;
        OpenOptions::new()
            .append(true)
            .create(true)
            .open(&self.log)
            .and_then(|mut log| log.write_all(&line))
    }

    /// The last `limit` entries that match `filter`, oldest first.
    ///
    /// The log is read backwards a block at a time and only as far as needed, so a long log costs
    /// no more than its tail, and no line is gathered beyond [`LONGEST_ENTRY`], so a damaged log
    /// costs no more memory than a sound one. Lines that are not entries are skipped and counted.
    ///
    /// A filter that matches little can still read the whole log, so `cancel` is checked before
    /// every block, and a cancelled read fails with [`io::ErrorKind::Interrupted`].
    pub fn tail(
        &self,
        limit: usize,
        filter: &Filter,
        cancel: &CancellationToken,
    ) -> io::Result<Tail> {
        let _held = self.hold(true)?;
        let mut file = match File::open(&self.log) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Tail::default()),
            Err(error) => return Err(error),
        };

        let mut tail = Tail::default();
        let mut buffer = vec![0; BLOCK];
        // The end of the earliest line found so far, whose start is in a block not read yet; `None`
        // while that line has grown past LONGEST_ENTRY and is being passed over.
        let mut carried: Option<Vec<u8>> = Some(Vec::new());
        let mut end = file.metadata()?.len();
        'blocks: while end > 0 {
            if cancel.is_cancelled() {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
            }
            let start = end.saturating_sub(BLOCK as u64);
            let block = &mut buffer[..usize::try_from(end - start).unwrap_or(BLOCK)];
            file.seek(SeekFrom::Start(start))?;
            file.read_exact(block)?;
            end = start;

            // Newest first. The first part ends the line carried from the block after this one; the
            // last begins in the block before, unless this block starts the file.
            let parts: Vec<&[u8]> = block.rsplit(|byte| *byte == b'\n').collect();
            let earliest = parts.len() - 1;
            for (index, part) in parts.into_iter().enumerate() {
                let line: Option<Cow<'_, [u8]>> = if index == 0 {
                    carried.take().and_then(|after| {
                        (part.len() + after.len() <= LONGEST_ENTRY).then(|| {
                            let mut line = part.to_vec();
                            line.extend_from_slice(&after);
                            Cow::Owned(line)
                        })
                    })
                } else {
                    (part.len() <= LONGEST_ENTRY).then_some(Cow::Borrowed(part))
                };
                if index == earliest && start > 0 {
                    carried = line.map(Cow::into_owned);
                    break;
                }
                let Some(line) = line else {
                    tail.skipped += 1;
                    continue;
                };
                let line = line.strip_suffix(b"\r").unwrap_or(&line);
                if line.iter().all(u8::is_ascii_whitespace) {
                    continue;
                }
                let Ok(entry) = serde_json::from_slice::<Value>(line) else {
                    tail.skipped += 1;
                    continue;
                };
                if !filter.matches(&entry) {
                    continue;
                }
                if tail.entries.len() == limit {
                    tail.truncated = true;
                    break 'blocks;
                }
                tail.entries.push(entry);
            }
        }
        tail.entries.reverse();
        Ok(tail)
    }
}

const CONTENT_KEYS: [&str; 8] = [
    "content", "contents", "data", "bytes", "body", "stdin", "old", "new",
];
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
/// Keys that are a secret only when they are the whole key, after `-` is read as `_`: as part of a
/// longer key they mean too many other things.
const SECRET_KEYS: [&str; 4] = ["auth", "pass", "pw", "pwd"];
/// Endings that make a key a secret, after `-` is read as `_`: `signing_key`, `basic_auth`.
const SECRET_KEY_ENDINGS: [&str; 3] = ["_key", "_auth", "_pass"];
/// Flags whose value is a secret besides those named like a secret key: `-p` takes a password for
/// `sshpass` and others, and `-u`/`--user` takes `user:password` for `curl`.
const SECRET_FLAGS: [&str; 3] = ["p", "u", "user"];
/// How a token from a well-known service starts, as a word: GitHub, GitLab, OpenAI, Slack, AWS,
/// Google, npm, PyPI, Hugging Face, Stripe.
const TOKEN_PREFIXES: [&str; 23] = [
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "github_pat_",
    "glpat-",
    "sk-",
    "xoxa-",
    "xoxb-",
    "xoxp-",
    "xoxr-",
    "xoxs-",
    "AKIA",
    "ASIA",
    "AIza",
    "ya29.",
    "npm_",
    "pypi-",
    "hf_",
    "sk_live_",
    "rk_live_",
    "sk_test_",
];
/// What gives a secret away wherever it stands in a string, compared in lower case: a PEM block
/// such as a private key, and a bearer token.
const SECRET_MARKERS: [&str; 2] = ["-----begin", "bearer "];
/// `Authorization` schemes, which give a secret away at the start of a string, compared in lower
/// case. In the middle of one they are too often ordinary words.
const AUTH_SCHEMES: [&str; 4] = ["basic ", "digest ", "bearer ", "token "];
/// The ways a string can show it carries a secret.
const STRING_RULES: [fn(&str) -> bool; 5] = [
    has_a_marker,
    has_a_token,
    assigns_a_secret,
    passes_a_secret_flag,
    has_credentials_in_a_url,
];
const LONGEST_KEPT: usize = 4096;
/// The most the arguments of one call take up in the log, as JSON; past it they are content,
/// however they are split up.
const LARGEST_ARGS: usize = 16 * 1024;

/// Arguments as the log records them.
pub fn redact(args: &Value) -> Value {
    let redacted = redact_value(None, args);
    if redacted.to_string().len() > LARGEST_ARGS {
        size_of(args)
    } else {
        redacted
    }
}

/// A failed call's message as the log records it: messages can echo the arguments they are about.
pub fn redact_message(message: &str) -> Value {
    redact_value(None, &Value::String(message.to_owned()))
}

/// What [`redact_command`] puts in place of a secret.
pub const REDACTED: &str = "[redacted]";

/// A command line's arguments as a person may be shown them, by the rules the log applies to
/// argument lists: the value after a secret flag (`--password x`, `-p x`), the value of a secret
/// assignment (`--token=x` keeps `--token=`), and any argument that gives a secret away by itself
/// (a known token, a URL with credentials, a bearer header) become [`REDACTED`].
pub fn redact_command(args: &[String]) -> Vec<String> {
    let mut follows_flag = false;
    args.iter()
        .map(|arg| {
            let shown = if follows_flag {
                REDACTED.to_owned()
            } else if let Some((name, _)) = arg.split_once('=')
                && (takes_secret(name) || is_sensitive_key(name.trim_start_matches('-')))
            {
                format!("{name}={REDACTED}")
            } else if looks_secret(arg) {
                REDACTED.to_owned()
            } else {
                arg.clone()
            };
            follows_flag = takes_secret(arg);
            shown
        })
        .collect()
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
            let mut follows_flag = false;
            Value::Array(
                items
                    .iter()
                    .map(|item| {
                        let recorded = if follows_flag {
                            size_of(item)
                        } else {
                            redact_value(None, item)
                        };
                        follows_flag = item.as_str().is_some_and(takes_secret);
                        recorded
                    })
                    .collect(),
            )
        }
        other => other.clone(),
    }
}

/// A key or a flag's name the way the rules compare it: lower case, with `-` read as `_`.
fn normalise(name: &str) -> String {
    name.to_ascii_lowercase().replace('-', "_")
}

fn is_sensitive_key(key: &str) -> bool {
    let key = normalise(key);
    CONTENT_KEYS.contains(&key.as_str())
        || SECRET_WORDS.iter().any(|word| key.contains(word))
        || SECRET_KEYS.contains(&key.as_str())
        || SECRET_KEY_ENDINGS
            .iter()
            .any(|ending| key.ends_with(ending))
}

/// Whether an argument is a flag whose value comes next and is a secret: `--password`, `-p`,
/// `--api-key`. A flag written `--password=…` carries its value itself, which
/// [`assigns_a_secret`] finds.
fn takes_secret(arg: &str) -> bool {
    let Some(flag) = arg.strip_prefix('-') else {
        return false;
    };
    let name = flag.strip_prefix('-').unwrap_or(flag);
    !name.is_empty()
        && !name.contains('=')
        && (SECRET_FLAGS.contains(&normalise(name).as_str()) || is_sensitive_key(name))
}

fn looks_secret(text: &str) -> bool {
    STRING_RULES.iter().any(|rule| rule(text))
}

fn has_a_marker(text: &str) -> bool {
    let lower = text.trim_start().to_ascii_lowercase();
    SECRET_MARKERS.iter().any(|marker| lower.contains(marker))
        || AUTH_SCHEMES.iter().any(|scheme| lower.starts_with(scheme))
}

/// A word that starts like a known token, or a JWT: three base64 parts, the first `eyJ`.
fn has_a_token(text: &str) -> bool {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')))
        .any(|word| {
            TOKEN_PREFIXES.iter().any(|prefix| word.starts_with(prefix))
                || (word.starts_with("eyJ") && word.contains('.'))
        })
}

/// `password=…`, `password = …`, `"api-key": …`, `?access_token=…`: a secret's name followed by
/// `=` or `:`, with spaces or quotes between.
fn assigns_a_secret(text: &str) -> bool {
    let text = normalise(text);
    SECRET_WORDS.iter().any(|word| {
        text.match_indices(word).any(|(at, _)| {
            text[at + word.len()..]
                .trim_start_matches(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '`'))
                .starts_with(['=', ':'])
        })
    })
}

/// A command line in one string that passes a secret flag its value: `mysql --password hunter2`.
fn passes_a_secret_flag(text: &str) -> bool {
    let mut words = text
        .split_whitespace()
        .map(|word| word.trim_matches(|c| matches!(c, '"' | '\'')));
    let mut previous = words.next();
    for word in words {
        if previous.is_some_and(takes_secret) && !word.is_empty() {
            return true;
        }
        previous = Some(word);
    }
    false
}

/// `scheme://user:password@host`, or a token as the user: `https://ghp_…@github.com`.
fn has_credentials_in_a_url(text: &str) -> bool {
    text.match_indices("://").any(|(at, _)| {
        let rest = &text[at + 3..];
        let authority = rest
            .split(|c: char| c.is_whitespace() || matches!(c, '/' | '?' | '#' | '"' | '\''))
            .next()
            .unwrap_or_default();
        authority
            .rsplit_once('@')
            .is_some_and(|(user, _)| user.contains(':') || has_a_token(user))
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
            "old": "before",
            "new": "afterwards",
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
        assert_eq!(redacted["old"], json!({"redacted_bytes": 6}));
        assert_eq!(redacted["new"], json!({"redacted_bytes": 10}));
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

    /// Arguments under `x`, as the log would record them.
    fn recorded(value: Value) -> Value {
        redact(&json!({ "x": value }))["x"].clone()
    }

    fn is_redacted(value: &Value) -> bool {
        value.get("redacted_bytes").is_some()
    }

    #[test]
    fn strings_that_carry_a_secret_anywhere_are_redacted() {
        for secret in [
            "Bearer abc",
            "bearer abc",
            "BASIC dXNlcjpwYXNz",
            "Authorization: Bearer abc",
            "https://me:hunter2@example.com/repo.git",
            "https://ghp_abc@github.com/me/repo",
            "https://example.com/api?token=abc",
            "https://example.com/api?x=1&access_token=abc",
            "PASSWORD=hunter2",
            "password = hunter2",
            "the db password: hunter2",
            r#"{"password": "hunter2"}"#,
            "eyJhbGciOi.eyJzdWIiOi.c2lnbmF0dXJl\n",
            "jwt eyJhbGciOi.eyJzdWIiOi.c2lnbmF0dXJl",
            "mysql --password hunter2 db",
            "curl -u me:hunter2 https://example.com",
            "gh auth login --with-token ghp_abc",
            "export GH=ghp_abc",
            "  -----BEGIN OPENSSH PRIVATE KEY-----",
        ] {
            assert!(is_redacted(&recorded(json!(secret))), "{secret:?} was kept");
        }
        for plain in [
            "/home/me/projects/app",
            "build-7",
            "ssh://me@tower/srv",
            "https://example.com/docs?page=2",
            "tokens are counted",
            "a basic fix",
            "eyJ is how a JWT starts",
            r"C:\Users\me\notes.txt",
        ] {
            assert_eq!(recorded(json!(plain)), json!(plain));
        }
    }

    #[test]
    fn the_value_after_a_secret_flag_in_an_argument_list_is_redacted() {
        for (argv, secret) in [
            (json!(["mysql", "--password", "hunter2", "db"]), 2),
            (json!(["sshpass", "-p", "hunter2", "ssh", "tower"]), 2),
            (json!(["gh", "--token", "abc"]), 2),
            (json!(["tool", "--api-key", "abc"]), 2),
            (json!(["tool", "--API_KEY", "abc"]), 2),
            (
                json!(["curl", "-u", "me:hunter2", "https://example.com"]),
                2,
            ),
            (json!(["curl", "--user", "me:hunter2"]), 2),
        ] {
            let kept = recorded(argv.clone());
            assert!(is_redacted(&kept[secret]), "{argv} kept {kept}");
            assert_eq!(kept[0], argv[0]);
            assert_eq!(kept[secret - 1], argv[secret - 1]);
        }
        let kept = recorded(json!(["ls", "-l", "/home/me"]));
        assert_eq!(kept, json!(["ls", "-l", "/home/me"]));
    }

    #[test]
    fn keys_that_name_a_secret_are_redacted_however_they_are_spelt() {
        for key in [
            "X-API-Key",
            "x_api_key",
            "apiKey",
            "api-key",
            "client-secret",
            "Authorization",
            "private-key",
            "GITHUB_TOKEN",
            "db_password",
            "signing_key",
            "auth",
        ] {
            let args = redact(&json!({ key: "x" }));
            assert!(is_redacted(&args[key]), "{key} was kept");
        }
        for key in ["key", "path", "author", "monkey", "keyboard"] {
            assert_eq!(redact(&json!({ key: "x" }))[key], json!("x"), "{key}");
        }
    }

    #[test]
    fn arguments_too_large_in_all_are_replaced_by_their_size() {
        let many: Vec<String> = (0..100)
            .map(|n| format!("{n}{}", "m".repeat(1000)))
            .collect();
        let args = redact(&json!({"argv": many}));
        assert!(is_redacted(&args), "{}", args.to_string().len());
        assert!(args.to_string().len() < LONGEST_KEPT);
    }

    #[test]
    fn a_message_that_echoes_a_secret_is_redacted() {
        assert!(is_redacted(&redact_message(
            "mysql failed: --password hunter2 was refused"
        )));
        assert_eq!(
            redact_message("There is no tool called fs.nope on tower."),
            json!("There is no tool called fs.nope on tower.")
        );
    }

    #[test]
    fn tail_skips_and_counts_lines_too_long_or_broken_without_reading_them_whole() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let log = AuditLog::new(&paths);
        log.append(&entry("meta.first", "owner", Outcome::Ok))
            .unwrap();
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(paths.audit_log())
            .unwrap();
        // Garbage with no newline at all, longer than many blocks.
        file.write_all(&vec![b'x'; 8 * 1024 * 1024]).unwrap();
        file.write_all(b"\n").unwrap();
        // A line that is JSON, but longer than any entry the log writes.
        let long = json!({"tool": "meta.long", "pad": "p".repeat(LONGEST_ENTRY)});
        file.write_all(format!("{long}\n").as_bytes()).unwrap();
        file.write_all(b"not json\n").unwrap();
        drop(file);
        log.append(&entry("meta.last", "owner", Outcome::Ok))
            .unwrap();

        let tail = log
            .tail(10, &Filter::default(), &CancellationToken::new())
            .unwrap();
        let tools: Vec<&str> = tail
            .entries
            .iter()
            .filter_map(|e| e["tool"].as_str())
            .collect();
        assert_eq!(tools, ["meta.first", "meta.last"]);
        assert_eq!(tail.skipped, 3);
        assert!(!tail.truncated);
    }

    #[test]
    fn a_tail_that_fails_gives_the_lock_back() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        std::fs::create_dir_all(paths.audit_log()).unwrap();
        let log = AuditLog::new(&paths);
        assert!(
            log.tail(1, &Filter::default(), &CancellationToken::new())
                .is_err()
        );
        let lock = File::create(paths.audit_lock()).unwrap();
        assert!(lock.try_lock().is_ok());
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

    #[test]
    fn a_command_line_keeps_its_shape_and_loses_its_secrets() {
        let args: Vec<String> = [
            "mysql",
            "--user",
            "me",
            "--password",
            "hunter2",
            "--api-key=abc123",
            "--host=db",
            "https://me:pw@example.com/x",
            "ghp_0123456789abcdef",
            "plain",
        ]
        .iter()
        .map(|arg| (*arg).to_owned())
        .collect();
        assert_eq!(
            redact_command(&args),
            [
                "mysql",
                "--user",
                REDACTED,
                "--password",
                REDACTED,
                "--api-key=[redacted]",
                "--host=db",
                REDACTED,
                REDACTED,
                "plain",
            ]
        );
    }
}
