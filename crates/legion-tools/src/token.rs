//! `token`: the bearer tokens `legion serve` accepts, kept in `tokens.toml`.
//!
//! A token is made here and shown once, in the reply to `token.add`; the file keeps only its hash.
//! The reply is the one place the token exists, so it is never written anywhere else: the audit
//! log records a call's arguments and outcome, never its result.

use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jiff::{SignedDuration, Timestamp};
use legion_core::config::{parse, write_atomically};
use legion_core::policy::Principal;
use legion_core::proto::{Effect, ErrorBody, ErrorCode};
use legion_core::tokens::{self, Tokens};
use legion_core::{Ctx, NoArgs, Registry, ToolError};
use legion_os::OsError;
use legion_os::lock::{FileLock, LockMode};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use toml_edit::{DocumentMut, Item, Table};

use crate::caller::Caller;

/// How many random bytes a token carries: as many as the hash that stands for it.
const TOKEN_BYTES: usize = 32;

/// The longest a token may be made to last, so a forgotten one does not work for ever by accident;
/// a token meant to last is made without `expires_days`.
const LONGEST_DAYS: u32 = 3650;

/// The longest a call waits for another that is changing `tokens.toml`.
const LOCK_TIMEOUT: Duration = Duration::from_secs(10);

/// Adds the `token` tools.
pub fn register(registry: &mut Registry) {
    registry.add_document::<Tokens>();
    registry.add_fn(
        "token.add",
        Effect::Admin,
        "Make a bearer token for `legion serve`, standing for one principal that policy.toml names and the caller could act as itself: its own, or any when the caller has the owner role. The token is in this reply and nowhere else: only its hash is kept, so copy it now. A token for a principal with the owner role is the keys to the machine (whoever holds it may do anything the account can, from anywhere the server is published), so only a caller with the owner role may make one, and only with allow_owner true. A token made by a call that came with an expiring token expires no later.",
        add,
    );
    registry.add_fn(
        "token.list",
        Effect::Admin,
        "List the tokens `legion serve` accepts for principals the caller could act as: each one's name, principal, when it was made and when it expires. The tokens themselves are not kept, so they cannot be shown.",
        list,
    );
    registry.add_fn(
        "token.remove",
        Effect::Admin,
        "Remove a token of a principal the caller could act as, so `legion serve` refuses it from its next request on and ends its MCP sessions.",
        remove,
    );
}

/// Arguments of `token.add`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct AddArgs {
    /// What to call the token, such as phone: lower-case letters, digits, - and _.
    name: String,
    /// The principal every request with the token acts as. policy.toml must name it, which is
    /// what gives it a role.
    principal: String,
    /// Days until the token stops working; it never does when left out.
    #[serde(default)]
    expires_days: Option<u32>,
    /// Allow a token for owner, which may do everything. Off unless given as true.
    #[serde(default)]
    allow_owner: bool,
}

/// What `token.add` returns.
#[derive(Debug, Serialize)]
struct Added {
    name: String,
    principal: String,
    /// The token: send it as `Authorization: Bearer <token>`.
    token: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    expires: Option<String>,
}

async fn add(ctx: &Ctx, args: AddArgs) -> Result<Added, ToolError> {
    tokens::check_name(&args.name)
        .map_err(|message| ErrorBody::new(ErrorCode::InvalidArgs, format!("name: {message}.")))?;
    let principal = Principal::parse(&args.principal)?;
    let principal = principal.as_str().to_owned();
    let caller = Caller::of(ctx)?;
    caller.check_issue(
        &principal,
        args.allow_owner,
        "a token",
        format!(
            "{principal} has the owner role: a token for it may do everything the account can, from anywhere the server is published, so it is not made unless asked for in so many words."
        ),
        "Give the token a principal with a narrower role in policy.toml, or pass allow_owner=true if this is really what you want.",
    )?;
    if let Some(days) = args.expires_days
        && !(1..=LONGEST_DAYS).contains(&days)
    {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("expires_days must be between 1 and {LONGEST_DAYS}."),
        )
        .with_hint("Leave it out for a token that does not expire."));
    }

    let now = Timestamp::from_second(Timestamp::now().as_second()).map_err(internal)?;
    let expires = match args.expires_days {
        Some(days) => Some(
            now.checked_add(SignedDuration::from_hours(i64::from(days) * 24))
                .map_err(internal)?,
        ),
        None => None,
    };
    // A token made through a token lasts no longer than it; no expiry is the longest of all.
    if let Some(limit) = ctx.credential_expires()
        && expires.is_none_or(|expires| expires > limit)
    {
        return Err(ErrorBody::new(
            ErrorCode::Denied,
            format!(
                "The token this call came with expires at {}, and a token it makes may not outlive it.",
                rfc3339(limit)
            ),
        )
        .with_hint("Give expires_days short enough to end by then, or have the owner of this machine make the token."));
    }

    let token = new_token()?;
    let entry = Entry {
        principal: principal.clone(),
        hash: tokens::hash(&token),
        created: rfc3339(now),
        expires: expires.map(rfc3339),
    };
    let name = args.name.clone();
    let file = ctx.config().path_of::<Tokens>();
    let lock = lock_path(ctx);
    blocking(move || {
        update(&file, &lock, |current, doc| {
            if current.tokens.contains_key(&name) {
                return Err(ErrorBody::new(
                    ErrorCode::Exists,
                    format!("There is already a token called {name}."),
                )
                .with_hint(
                    "Choose another name, or remove the old token first with token.remove.",
                ));
            }
            insert(doc, &name, &entry);
            Ok(true)
        })
    })
    .await?;
    Ok(Added {
        name: args.name,
        principal,
        token,
        expires: expires.map(rfc3339),
    })
}

/// What `token.list` returns.
#[derive(Debug, Serialize)]
struct Listing {
    tokens: Vec<Listed>,
}

/// One token as `token.list` shows it. Never its hash, which is of no use to anyone reading the
/// list and is one step closer to the token than a name.
#[derive(Debug, Serialize)]
struct Listed {
    name: String,
    principal: String,
    created: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    expires: Option<String>,
    expired: bool,
}

async fn list(ctx: &Ctx, _args: NoArgs) -> Result<Listing, ToolError> {
    let caller = Caller::of(ctx)?;
    let file = ctx.config().get::<Tokens>()?;
    let now = Timestamp::now();
    let tokens = file
        .tokens
        .iter()
        .filter(|(_, entry)| caller.may_manage(entry.principal.as_str()))
        .map(|(name, entry)| Listed {
            name: name.clone(),
            principal: entry.principal.as_str().to_owned(),
            created: entry.created.clone(),
            expires: entry.expires.clone(),
            expired: entry
                .expires
                .as_deref()
                .and_then(|expires| tokens::parse_time(expires).ok())
                .is_some_and(|expires| now >= expires),
        })
        .collect();
    Ok(Listing { tokens })
}

/// Arguments of `token.remove`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RemoveArgs {
    /// The token's name, as token.list shows it.
    name: String,
}

/// What `token.remove` returns.
#[derive(Debug, Serialize)]
struct Removed {
    name: String,
    removed: bool,
}

async fn remove(ctx: &Ctx, args: RemoveArgs) -> Result<Removed, ToolError> {
    let name = args.name.clone();
    let file = ctx.config().path_of::<Tokens>();
    let lock = lock_path(ctx);
    // Which tokens the caller may see, decided before the file is locked: a token of a principal
    // it may not act as is treated exactly like one that does not exist.
    let caller = Caller::of(ctx)?;
    blocking(move || {
        update(&file, &lock, |current, doc| {
            let visible = |entry: &tokens::TokenEntry| caller.may_manage(entry.principal.as_str());
            if !current.tokens.get(&name).is_some_and(visible) {
                let known: Vec<&str> = current
                    .tokens
                    .iter()
                    .filter(|(_, entry)| visible(entry))
                    .map(|(name, _)| name.as_str())
                    .collect();
                let hint = if known.is_empty() {
                    "There are no tokens.".to_owned()
                } else {
                    format!("The tokens are {}.", known.join(", "))
                };
                return Err(ErrorBody::new(
                    ErrorCode::NotFound,
                    format!("There is no token called {name}."),
                )
                .with_hint(hint));
            }
            remove_entry(doc, &name);
            Ok(true)
        })
    })
    .await?;
    Ok(Removed {
        name: args.name,
        removed: true,
    })
}

/// A token as the file will keep it.
struct Entry {
    principal: String,
    hash: String,
    created: String,
    expires: Option<String>,
}

fn insert(doc: &mut DocumentMut, name: &str, entry: &Entry) {
    let root = doc.as_table_mut();
    if !root.get("tokens").is_some_and(Item::is_table_like) {
        let mut tokens = Table::new();
        // Without this the file would gain an empty `[tokens]` header above the first token.
        tokens.set_implicit(true);
        root.insert("tokens", Item::Table(tokens));
    }
    let mut table = Table::new();
    // A file of comments alone keeps them as trailing text, which would print after the first
    // table; they open the file, so they go above it.
    let has_entries = root
        .get("tokens")
        .and_then(Item::as_table_like)
        .is_some_and(|tokens| !tokens.is_empty());
    if !has_entries && root.len() <= 1 {
        let opening = doc_trailing(doc);
        table.decor_mut().set_prefix(opening);
    }
    let root = doc.as_table_mut();
    table.insert("principal", toml_edit::value(entry.principal.as_str()));
    table.insert("hash", toml_edit::value(entry.hash.as_str()));
    table.insert("created", toml_edit::value(entry.created.as_str()));
    if let Some(expires) = &entry.expires {
        table.insert("expires", toml_edit::value(expires.as_str()));
    }
    if let Some(tokens) = root.get_mut("tokens").and_then(Item::as_table_like_mut) {
        tokens.insert(name, Item::Table(table));
    }
}

/// Takes the text after the last table, leaving none.
fn doc_trailing(doc: &mut DocumentMut) -> String {
    let text = doc.trailing().as_str().unwrap_or_default().to_owned();
    doc.set_trailing("");
    text
}

/// Removes a token's table, keeping the comments above it that a blank line sets apart from it.
///
/// toml_edit hangs every comment above a table header on that table, so the comment that opens
/// the file belongs to the first token. Text a blank line separates from the header is about the
/// file or what comes before, not the token, so it moves to the next table, or to the end of the
/// file when none follows.
fn remove_entry(doc: &mut DocumentMut, name: &str) {
    let Some(tokens) = doc.get_mut("tokens").and_then(Item::as_table_mut) else {
        if let Some(tokens) = doc.get_mut("tokens").and_then(Item::as_table_like_mut) {
            tokens.remove(name);
        }
        return;
    };
    let Some(Item::Table(removed)) = tokens.remove(name) else {
        return;
    };
    let prefix = removed
        .decor()
        .prefix()
        .and_then(|prefix| prefix.as_str())
        .unwrap_or_default();
    let lines: Vec<&str> = prefix.split_inclusive('\n').collect();
    let kept = match lines.iter().rposition(|line| line.trim().is_empty()) {
        Some(last_blank) => lines[..=last_blank].concat(),
        None => String::new(),
    };
    if !kept.contains('#') {
        return;
    }
    let after = removed.position();
    let next = tokens
        .iter_mut()
        .filter_map(|(_, item)| item.as_table_mut())
        .filter(|table| table.position() > after)
        .min_by_key(|table| table.position());
    match next {
        Some(table) => {
            let rest = table
                .decor()
                .prefix()
                .and_then(|prefix| prefix.as_str())
                .unwrap_or_default()
                .to_owned();
            table.decor_mut().set_prefix(format!("{kept}{rest}"));
        }
        None => {
            let rest = doc.trailing().as_str().unwrap_or_default().to_owned();
            doc.set_trailing(format!("{kept}{rest}"));
        }
    }
}

/// Changes `tokens.toml` under a kernel lock, so two calls changing it at once cannot lose each
/// other's change.
///
/// `edit` sees the file as it is once the lock is held, parsed and as editable text, and says
/// whether it changed anything. The new text must parse as the document before it replaces the
/// file, atomically, so comments and layout a person gave the file survive and no reader ever sees
/// half a file. A file that does not parse is never touched: whoever broke it is the one to mend it.
fn update(
    file: &Path,
    lock: &Path,
    edit: impl FnOnce(&Tokens, &mut DocumentMut) -> Result<bool, ErrorBody>,
) -> Result<(), ErrorBody> {
    let failed = |what: String, error: &dyn std::fmt::Display| {
        ErrorBody::new(ErrorCode::Failed, format!("{what}: {error}."))
    };
    if let Some(dir) = lock.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|error| failed(format!("{} cannot be created", dir.display()), &error))?;
    }
    let _held =
        FileLock::acquire_timeout(lock, LockMode::Exclusive, LOCK_TIMEOUT).map_err(|error| {
            match error {
                OsError::TimedOut { .. } => ErrorBody::new(
                    ErrorCode::Busy,
                    "Another call has been changing tokens.toml for too long.",
                )
                .with_hint("Try again in a moment."),
                other => failed(format!("{} cannot be locked", lock.display()), &other),
            }
        })?;
    let bytes = match std::fs::read(file) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(failed(format!("{} cannot be read", file.display()), &error)),
    };
    let current = match &bytes {
        Some(bytes) => parse::<Tokens>(file, bytes)?,
        None => Tokens::default(),
    };
    // parse() has just accepted the bytes as UTF-8.
    let text = String::from_utf8_lossy(bytes.as_deref().unwrap_or_default());
    let mut doc: DocumentMut = text.parse().map_err(|error: toml_edit::TomlError| {
        failed(format!("{} does not parse", file.display()), &error)
    })?;
    if !edit(&current, &mut doc)? {
        return Ok(());
    }
    let edited = doc.to_string();
    parse::<Tokens>(file, edited.as_bytes())?;
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|error| failed(format!("{} cannot be created", dir.display()), &error))?;
    }
    write_atomically(file, edited.as_bytes())
        .map_err(|error| failed(format!("{} cannot be written", file.display()), &error))
}

/// `lgn_` and 32 random bytes from the operating system, in URL-safe base64 so the token can go in
/// a header, a URL or a shell without quoting.
fn new_token() -> Result<String, ErrorBody> {
    let mut bytes = [0_u8; TOKEN_BYTES];
    getrandom::fill(&mut bytes).map_err(|error| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("The operating system gave no random bytes for the token: {error}."),
        )
    })?;
    Ok(format!(
        "{}{}",
        tokens::PREFIX,
        URL_SAFE_NO_PAD.encode(bytes)
    ))
}

fn rfc3339(time: Timestamp) -> String {
    time.strftime("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn lock_path(ctx: &Ctx) -> PathBuf {
    ctx.paths().state_dir().join("tokens.lock")
}

fn internal(error: impl std::fmt::Display) -> ErrorBody {
    ErrorBody::new(
        ErrorCode::Internal,
        format!("The token's times cannot be worked out: {error}."),
    )
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, ErrorBody> + Send + 'static,
) -> Result<T, ErrorBody> {
    tokio::task::spawn_blocking(work).await.map_err(|error| {
        ErrorBody::new(
            ErrorCode::Internal,
            format!("A token tool stopped unexpectedly: {error}."),
        )
    })?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_is_the_prefix_and_43_url_safe_characters() {
        let token = new_token().unwrap();
        let body = token.strip_prefix("lgn_").unwrap();
        assert_eq!(body.len(), 43);
        assert!(
            body.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
        assert_ne!(new_token().unwrap(), token);
    }

    #[test]
    fn edits_keep_comments_and_the_file_parses_after_each() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("tokens.toml");
        let lock = dir.path().join("tokens.lock");
        std::fs::write(&file, "# Tokens for legion serve.\n\n").unwrap();
        let entry = |principal: &str| Entry {
            principal: principal.to_owned(),
            hash: tokens::hash(principal),
            created: "2026-01-01T00:00:00Z".to_owned(),
            expires: None,
        };
        update(&file, &lock, |_, doc| {
            insert(doc, "phone", &entry("phone"));
            Ok(true)
        })
        .unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.starts_with("# Tokens for legion serve.\n"), "{text}");
        assert!(text.contains("[tokens.phone]"), "{text}");
        assert!(!text.contains("[tokens]\n"), "{text}");
        let mut text = text;
        text.push_str("# The laptop's, for scripts.\n");
        std::fs::write(&file, &text).unwrap();
        update(&file, &lock, |_, doc| {
            insert(doc, "laptop", &entry("laptop"));
            Ok(true)
        })
        .unwrap();
        update(&file, &lock, |_, doc| {
            remove_entry(doc, "phone");
            Ok(true)
        })
        .unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.starts_with("# Tokens for legion serve.\n"), "{text}");
        assert!(text.contains("# The laptop's, for scripts."), "{text}");
        assert!(!text.contains("phone"), "{text}");
        let parsed = parse::<Tokens>(&file, text.as_bytes()).unwrap();
        assert_eq!(parsed.tokens.len(), 1);
        update(&file, &lock, |_, doc| {
            remove_entry(doc, "laptop");
            Ok(true)
        })
        .unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.starts_with("# Tokens for legion serve.\n"), "{text}");
        assert!(!text.contains("laptop\"]"), "{text}");
        assert!(
            parse::<Tokens>(&file, text.as_bytes())
                .unwrap()
                .tokens
                .is_empty()
        );
    }

    #[test]
    fn a_file_that_does_not_parse_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("tokens.toml");
        std::fs::write(&file, "[tokens.x\n").unwrap();
        let error = update(&file, &dir.path().join("lock"), |_, _| Ok(true)).unwrap_err();
        assert_eq!(error.code, ErrorCode::Failed);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "[tokens.x\n");
    }
}
