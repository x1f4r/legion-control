//! Who may call which tools, and on which paths.
//!
//! `policy.toml` names principals and gives each a role; a role allows and denies tools by name, by
//! glob or by effect, and may confine the file tools to a few directories:
//!
//! ```toml
//! default = "owner"               # the principal of a caller who names none
//!
//! [principals."agent:claude"]
//! role = "operator"
//!
//! [roles.operator]
//! allow = ["effect:read", "effect:write", "effect:exec"]
//! deny  = ["fs.remove", "power.*"]
//! paths = ["~/projects/**"]
//! ```
//!
//! Deny wins over allow. Two roles are built in: `owner`, which allows everything and may act as
//! anyone else, and `nobody`, which allows nothing and is what a principal the file does not name
//! gets. With no file at all the only principal is `owner`, so a fresh install behaves like the
//! shell it was started from.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use legion_proto::{Effect, ErrorBody, ErrorCode, ToolSpec};
use serde::Deserialize;
use serde_json::json;

use crate::config::Document;
use crate::glob;

/// The principal a local shell is, and the role that may do anything.
pub const OWNER: &str = "owner";
/// The role that may do nothing, given to every principal the policy does not name.
pub const NOBODY: &str = "nobody";

/// `policy.toml`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    /// The principal of a caller who names none; `owner` when absent.
    #[serde(default)]
    pub default: Option<Principal>,
    /// The principals this node knows, each with its role.
    #[serde(default)]
    pub principals: BTreeMap<Principal, PrincipalEntry>,
    /// The roles principals can be given, besides the built-in `owner` and `nobody`.
    #[serde(default)]
    pub roles: BTreeMap<String, Role>,
}

/// What the policy says about one principal.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrincipalEntry {
    /// The role it has.
    pub role: String,
}

/// A named set of rules.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Role {
    /// Tools this role may call.
    #[serde(default)]
    pub allow: Vec<Rule>,
    /// Tools it may not, whatever `allow` says.
    #[serde(default)]
    pub deny: Vec<Rule>,
    /// When present, the only directories the file tools may touch.
    #[serde(default)]
    pub paths: Option<Vec<PathRoot>>,
    /// Whether a caller with this role may act as another principal.
    #[serde(default)]
    pub delegate: bool,
}

impl Role {
    fn owner() -> Role {
        Role {
            allow: vec![Rule::Tools("*".to_owned())],
            deny: Vec::new(),
            paths: None,
            delegate: true,
        }
    }

    fn nobody() -> Role {
        Role::default()
    }
}

/// Who is calling: `owner`, `agent:claude`, `phone`. Letters, digits and `: . _ @ -`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(try_from = "String")]
pub struct Principal(String);

impl Principal {
    /// Checks a principal's name, as it would be checked in the file.
    pub fn parse(name: &str) -> Result<Principal, ErrorBody> {
        Principal::try_from(name.to_owned()).map_err(|message| {
            ErrorBody::new(ErrorCode::InvalidArgs, format!("{message}."))
                .with_hint("A principal is a name such as owner, phone or agent:claude.")
        })
    }

    /// The name as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Principal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for Principal {
    type Error = String;

    fn try_from(name: String) -> Result<Self, Self::Error> {
        let fits = |c: char| c.is_ascii_alphanumeric() || matches!(c, ':' | '.' | '_' | '@' | '-');
        if name.is_empty() || name.len() > 128 || !name.chars().all(fits) {
            Err(format!(
                "{name:?} is not a principal: use letters, digits and : . _ @ -"
            ))
        } else {
            Ok(Principal(name))
        }
    }
}

/// One allow or deny entry: a tool's name, a glob over names, or `effect:<effect>`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub enum Rule {
    /// Every tool with this effect.
    Effect(Effect),
    /// Every tool whose name the glob matches; a name without wildcards matches only itself.
    Tools(String),
}

impl Rule {
    /// Whether this rule covers the tool.
    pub fn matches(&self, spec: &ToolSpec) -> bool {
        match self {
            Rule::Effect(effect) => spec.effect == *effect,
            Rule::Tools(pattern) => glob::matches(pattern, &spec.name),
        }
    }
}

impl fmt::Display for Rule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Rule::Effect(effect) => write!(f, "effect:{effect}"),
            Rule::Tools(pattern) => f.write_str(pattern),
        }
    }
}

impl TryFrom<String> for Rule {
    type Error = String;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        if let Some(effect) = text.strip_prefix("effect:") {
            return effect.parse().map(Rule::Effect);
        }
        let fits = |c: char| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-' | '*' | '?')
        };
        if text.is_empty() || !text.chars().all(fits) {
            return Err(format!(
                "{text:?} is not a rule: write a tool name such as fs.read, a glob such as fs.*, or effect:<effect>"
            ));
        }
        Ok(Rule::Tools(text))
    }
}

/// A directory the file tools are confined to: absolute or starting at `~`. A trailing `/**`
/// says what is meant anyway, the directory and everything under it, and is accepted for that.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub struct PathRoot(String);

impl PathRoot {
    /// The root as written, without a trailing `/**`.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for PathRoot {
    type Error = String;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        let root = text
            .strip_suffix("/**")
            .or_else(|| text.strip_suffix("\\**"))
            .unwrap_or(&text);
        if root.contains(['*', '?', '[']) {
            return Err(format!(
                "{text:?} is not a path root: a root is a directory, optionally followed by /**, without other wildcards"
            ));
        }
        let from_home = root == "~" || root.starts_with("~/") || root.starts_with("~\\");
        if !from_home && !Path::new(root).is_absolute() {
            return Err(format!(
                "{text:?} is not a path root: write it from the root of the file system or from ~"
            ));
        }
        Ok(PathRoot(root.to_owned()))
    }
}

impl Document for Policy {
    const FILE: &'static str = "policy.toml";

    fn validate(&self) -> Result<(), String> {
        for builtin in [OWNER, NOBODY] {
            if self.roles.contains_key(builtin) {
                return Err(format!(
                    "[roles.{builtin}] cannot be defined: {builtin} is a built-in role"
                ));
            }
        }
        if self
            .principals
            .keys()
            .any(|principal| principal.as_str() == OWNER)
        {
            return Err(
                "[principals.owner] cannot be defined: owner is built in and has the role owner"
                    .to_owned(),
            );
        }
        for (principal, entry) in &self.principals {
            let known =
                entry.role == OWNER || entry.role == NOBODY || self.roles.contains_key(&entry.role);
            if !known {
                return Err(format!(
                    "principal {principal:?} has the role {:?}, which is neither built in nor defined under [roles]",
                    entry.role
                ));
            }
        }
        if let Some(default) = &self.default
            && default.as_str() != OWNER
            && !self.principals.contains_key(default)
        {
            return Err(format!(
                "default names {default:?}, which has no [principals] entry; give it one, with role = \"nobody\" if that is what is meant"
            ));
        }
        Ok(())
    }
}

impl Policy {
    /// The principal of a caller who names none.
    pub fn default_principal(&self) -> &str {
        self.default.as_ref().map_or(OWNER, Principal::as_str)
    }

    /// The name of a principal's role and its rules: `nobody` for a principal the file does not
    /// name.
    pub fn role_of(&self, principal: &str) -> (String, Role) {
        let role_name = if principal == OWNER {
            OWNER
        } else {
            self.principals
                .iter()
                .find(|(name, _)| name.as_str() == principal)
                .map_or(NOBODY, |(_, entry)| entry.role.as_str())
        };
        let role = match role_name {
            OWNER => Role::owner(),
            NOBODY => Role::nobody(),
            name => self.roles.get(name).cloned().unwrap_or_else(Role::nobody),
        };
        (role_name.to_owned(), role)
    }

    /// Who a call acts as.
    ///
    /// The caller is the principal the door pinned, or the policy's default when the door pinned
    /// none. A call that asks to act as someone else may, when the caller's role delegates. When
    /// it does not, a pinned door ignores the request, as the design says of a door that pinned a
    /// principal, and an unpinned one refuses it, since its caller could only be trying to widen
    /// what it may do.
    pub fn resolve(
        &self,
        requested: Option<&str>,
        pinned: Option<&str>,
    ) -> Result<String, ErrorBody> {
        let caller = match pinned {
            Some(pinned) => Principal::parse(pinned)?.0,
            None => self.default_principal().to_owned(),
        };
        let Some(requested) = requested else {
            return Ok(caller);
        };
        let requested = Principal::parse(requested)?.0;
        if requested == caller {
            return Ok(caller);
        }
        let (role, rules) = self.role_of(&caller);
        if rules.delegate {
            Ok(requested)
        } else if pinned.is_some() {
            Ok(caller)
        } else {
            Err(ErrorBody::new(
                ErrorCode::Denied,
                format!("{caller} may not act as {requested}: its role {role} does not delegate."),
            )
            .with_hint(
                "Call without --as, or give the caller's role delegate = true in policy.toml.",
            ))
        }
    }

    /// Whether `principal` may call the tool, and if so the grant its call carries.
    pub fn authorize(
        &self,
        principal: &str,
        spec: &ToolSpec,
        home: Option<&Path>,
    ) -> Result<Grant, ErrorBody> {
        let grant = self.grant(principal, home);
        let tool = &spec.name;
        let role = &grant.role;
        if let Some(rule) = grant.rules.deny.iter().find(|rule| rule.matches(spec)) {
            return Err(ErrorBody::new(
                ErrorCode::Denied,
                format!("{principal} may not use {tool}: its role {role} denies it with the rule {rule}."),
            )
            .with_hint("Ask the owner of this machine to change policy.toml if it should be allowed.")
            .with_details(json!({"principal": principal, "role": role, "tool": tool, "rule": rule.to_string()})));
        }
        if grant.rules.allow.iter().any(|rule| rule.matches(spec)) {
            return Ok(grant);
        }
        let (message, hint) = if role == NOBODY
            && !self.principals.keys().any(|p| p.as_str() == principal)
        {
            (
                format!(
                    "{principal} may not use {tool}: policy.toml does not name {principal}, so it has the role nobody, which allows nothing."
                ),
                format!(
                    "Add [principals.\"{principal}\"] with a role to policy.toml, or call without --as."
                ),
            )
        } else {
            (
                format!(
                    "{principal} may not use {tool}: its role {role} allows no rule that matches it (its effect is {}).",
                    spec.effect
                ),
                "Ask the owner of this machine to change policy.toml if it should be allowed."
                    .to_owned(),
            )
        };
        Err(ErrorBody::new(ErrorCode::Denied, message)
            .with_hint(hint)
            .with_details(json!({"principal": principal, "role": role, "tool": tool})))
    }

    /// The grant a principal has, whatever it calls; what `meta.whoami` reports.
    pub fn grant(&self, principal: &str, home: Option<&Path>) -> Grant {
        let (role, rules) = self.role_of(principal);
        Grant {
            principal: principal.to_owned(),
            role,
            rules: Arc::new(rules),
            home: home.map(Path::to_path_buf),
        }
    }
}

/// What a call is allowed, carried in its context: the principal, its role, and the paths it may
/// touch.
#[derive(Clone, Debug)]
pub struct Grant {
    principal: String,
    role: String,
    rules: Arc<Role>,
    home: Option<PathBuf>,
}

/// Whether a path is wanted for reading or for writing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// The tool will read it, list it or look at it.
    Read,
    /// The tool will create, change, move or remove it.
    Write,
}

impl Access {
    fn verb(self) -> &'static str {
        match self {
            Access::Read => "read",
            Access::Write => "write",
        }
    }
}

impl Grant {
    /// The grant of the owner, for a node with no policy at all.
    pub fn owner(home: Option<&Path>) -> Grant {
        Policy::default().grant(OWNER, home)
    }

    /// The principal the call acts as.
    pub fn principal(&self) -> &str {
        &self.principal
    }

    /// The name of its role.
    pub fn role(&self) -> &str {
        &self.role
    }

    /// Whether it may act as someone else.
    pub fn delegates(&self) -> bool {
        self.rules.delegate
    }

    /// Whether this grant's role lets it call the tool.
    pub fn allows(&self, spec: &ToolSpec) -> bool {
        !self.rules.deny.iter().any(|rule| rule.matches(spec))
            && self.rules.allow.iter().any(|rule| rule.matches(spec))
    }

    /// The directories file tools are confined to, as written in the policy, or `None` when they
    /// are not confined.
    pub fn roots(&self) -> Option<Vec<String>> {
        self.rules
            .paths
            .as_ref()
            .map(|roots| roots.iter().map(|root| root.as_str().to_owned()).collect())
    }

    /// Checks a path a tool was given and returns the one the tool should use.
    ///
    /// `~` is expanded and relative paths are refused. Every directory on the way is resolved,
    /// symbolic links and `..` included, so the result names the same file the kernel would open;
    /// the last part is kept as given when it is a symbolic link, so a tool that removes or renames
    /// one acts on the link and not on what it points to.
    ///
    /// When the role confines file tools to `paths`, the result must lie under one of them, and so
    /// must the file a symbolic link in the last part leads to: a link inside a root cannot be a
    /// way out of it. A path whose parent directories do not exist yet is checked as it will be
    /// once they do, and `..` among those is refused, since nothing can say where it would lead.
    /// Whatever cannot be told for sure is refused rather than guessed at: a symbolic link on the
    /// way that leads nowhere, a directory that cannot be looked into, and on Windows a path that
    /// names an alternate data stream (`file:stream`). On macOS and Windows, whose file systems
    /// usually ignore case, names that do not exist yet are compared ignoring case too.
    ///
    /// The answer holds when it is given. A tool that uses the path a moment later uses whatever is
    /// there then, so another process that swaps a directory on the way for a symbolic link in
    /// between is not defended against: confinement keeps a confined caller to its own roots, it
    /// does not protect the machine from a local attacker racing it.
    pub fn check_path(&self, path: impl AsRef<Path>, access: Access) -> Result<PathBuf, ErrorBody> {
        let given = path.as_ref();
        let expanded = expand_tilde(given, self.home.as_deref())?;
        if !expanded.is_absolute() {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!(
                    "{} is a relative path, and Legion takes only absolute ones.",
                    given.display()
                ),
            )
            .with_hint("Write the path from the root of the file system, or from ~."));
        }
        if names_a_stream(expanded.as_os_str().as_encoded_bytes(), Platform::HOST) {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!(
                    "{} names an alternate data stream, which Legion does not open.",
                    given.display()
                ),
            )
            .with_hint("Name the file itself: on Windows a colon after the drive selects a stream inside a file."));
        }

        let Some(roots) = &self.rules.paths else {
            return resolve_keeping_last(&expanded)
                .map(|resolved| resolved.path())
                .map_err(|error| error.unconfined(given));
        };
        let denied = |reason: &str| {
            ErrorBody::new(
                ErrorCode::Denied,
                format!(
                    "{} may not {} {}: {reason}.",
                    self.principal,
                    access.verb(),
                    given.display()
                ),
            )
        };
        let resolved = resolve_keeping_last(&expanded).map_err(|error| match error {
            Unresolved::Invalid(error) => error,
            Unresolved::Unclear(_, reason) => denied(&reason).with_hint(
                "A confined role may only use a path that resolves, link by link, to a place under its roots.",
            ),
        })?;
        let mut resolved_roots = Vec::with_capacity(roots.len());
        for root in roots {
            let expanded = expand_tilde(Path::new(root.as_str()), self.home.as_deref())?;
            let resolved_root = resolve(&expanded).map_err(|error| {
                let reason = match error {
                    Unresolved::Invalid(error) => error.message,
                    Unresolved::Unclear(_, reason) => reason,
                };
                denied(&format!("its role's root {} cannot be resolved ({reason})", root.as_str()))
                    .with_hint("Ask the owner of this machine to fix the role's paths in policy.toml, or the directory they name.")
            })?;
            resolved_roots.push(resolved_root);
        }
        let inside = |path: &Resolved| {
            resolved_roots
                .iter()
                .any(|root| within(path, root, Platform::HOST))
        };
        let outside = || {
            denied(&format!(
                "its role {} confines file tools to {}",
                self.role,
                roots.iter().map(PathRoot::as_str).collect::<Vec<_>>().join(", ")
            ))
            .with_hint("Use a path under one of those directories, or ask the owner to add the directory to the role's paths in policy.toml.")
            .with_details(json!({
                "path": resolved.path(),
                "roots": resolved_roots.iter().map(Resolved::path).collect::<Vec<_>>(),
            }))
        };
        if !inside(&resolved) {
            return Err(outside());
        }
        // A last part that is a symbolic link was kept as given; where it leads must be inside as
        // well. A link that leads nowhere cannot be checked, and writing through it would create a
        // file wherever it points.
        if resolved.link.is_some() {
            let target = dunce::canonicalize(resolved.path()).map(Resolved::existing);
            if !target.is_ok_and(|target| inside(&target)) {
                return Err(outside());
            }
        }
        Ok(resolved.path())
    }
}

/// Expands a leading `~` to the home directory. `~user` is refused rather than guessed at.
pub fn expand_tilde(path: &Path, home: Option<&Path>) -> Result<PathBuf, ErrorBody> {
    let mut components = path.components();
    let Some(Component::Normal(first)) = components.next() else {
        return Ok(path.to_path_buf());
    };
    if first == "~" {
        let home = home.ok_or_else(|| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!(
                    "{} starts at ~, and this node does not know its home directory.",
                    path.display()
                ),
            )
            .with_hint("Write the full path, or set HOME (USERPROFILE on Windows) for legion.")
        })?;
        return Ok(home.join(components.as_path()));
    }
    if first.to_string_lossy().starts_with('~') {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!(
                "{} names another user's home, which Legion does not expand.",
                path.display()
            ),
        )
        .with_hint("Write the full path."));
    }
    Ok(path.to_path_buf())
}

/// A path resolved as far as it exists.
#[derive(Clone, Debug)]
struct Resolved {
    /// The deepest part that exists, canonicalised, so its names are spelt as they are on disk.
    existing: PathBuf,
    /// The names below it that do not exist yet, as written.
    missing: Vec<OsString>,
    /// A last part that is a symbolic link, as written: kept rather than followed, so a tool acts on
    /// the link itself.
    link: Option<OsString>,
}

impl Resolved {
    fn existing(path: PathBuf) -> Resolved {
        Resolved {
            existing: path,
            missing: Vec::new(),
            link: None,
        }
    }

    fn path(&self) -> PathBuf {
        let mut path = self.existing.clone();
        path.extend(&self.missing);
        if let Some(link) = &self.link {
            path.push(link);
        }
        path
    }
}

/// Why a path could not be resolved.
#[derive(Debug)]
enum Unresolved {
    /// The path is wrong as written, whoever asks.
    Invalid(ErrorBody),
    /// Something on the way leads nowhere or cannot be looked at: a confined caller is refused, and
    /// anyone else gets the code with the reason.
    Unclear(ErrorCode, String),
}

impl Unresolved {
    fn unconfined(self, given: &Path) -> ErrorBody {
        match self {
            Unresolved::Invalid(error) => error,
            Unresolved::Unclear(code, reason) => ErrorBody::new(
                code,
                format!("{} cannot be used: {reason}.", given.display()),
            ),
        }
    }
}

/// Resolves every part of an absolute path that exists, symbolic links and `..` included, and
/// appends the parts that do not exist yet as they are.
///
/// Only a part that is genuinely absent may be appended unresolved. Each part is looked at without
/// following it, so a symbolic link that leads nowhere is found as the link it is, not taken for a
/// directory that does not exist yet, and anything that cannot be looked at stops the walk.
fn resolve(path: &Path) -> Result<Resolved, Unresolved> {
    let mut existing = path.to_path_buf();
    // The parts that do not exist, the last one first.
    let mut missing: Vec<OsString> = Vec::new();
    loop {
        match std::fs::symlink_metadata(&existing) {
            Ok(_) => {
                return match dunce::canonicalize(&existing) {
                    Ok(resolved) => {
                        missing.reverse();
                        Ok(Resolved {
                            existing: resolved,
                            missing,
                            link: None,
                        })
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        Err(Unresolved::Unclear(
                            ErrorCode::NotFound,
                            format!(
                                "{} is a symbolic link that leads nowhere",
                                existing.display()
                            ),
                        ))
                    }
                    Err(error) => Err(Unresolved::Unclear(
                        ErrorCode::Failed,
                        format!("{} cannot be resolved ({error})", existing.display()),
                    )),
                };
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                match existing.components().next_back() {
                    Some(Component::Normal(name)) => missing.push(name.to_owned()),
                    Some(Component::ParentDir) => {
                        return Err(Unresolved::Invalid(ErrorBody::new(
                            ErrorCode::InvalidArgs,
                            format!(
                                "{} goes up with .. out of a directory that does not exist.",
                                path.display()
                            ),
                        )
                        .with_hint("Write the path without .., the way it will be once its directories exist.")));
                    }
                    _ => {
                        return Err(Unresolved::Invalid(ErrorBody::new(
                            ErrorCode::NotFound,
                            format!(
                                "{} is on a drive or volume that does not exist.",
                                path.display()
                            ),
                        )));
                    }
                }
                existing.pop();
            }
            Err(error) => {
                return Err(Unresolved::Unclear(
                    ErrorCode::Failed,
                    format!("{} cannot be looked at ({error})", existing.display()),
                ));
            }
        }
    }
}

/// [`resolve`] for every part but the last, which is resolved too unless it is a symbolic link.
fn resolve_keeping_last(path: &Path) -> Result<Resolved, Unresolved> {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return resolve(path);
    };
    let mut resolved = resolve(parent)?;
    if !resolved.missing.is_empty() {
        resolved.missing.push(name.to_owned());
        return Ok(resolved);
    }
    let last = resolved.existing.join(name);
    match std::fs::symlink_metadata(&last) {
        Ok(meta) if meta.file_type().is_symlink() => resolved.link = Some(name.to_owned()),
        // Resolved like the rest, so it is spelt as it is on disk.
        Ok(_) => resolved = resolve(&last)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            resolved.missing.push(name.to_owned());
        }
        Err(error) => {
            return Err(Unresolved::Unclear(
                ErrorCode::Failed,
                format!("{} cannot be looked at ({error})", last.display()),
            ));
        }
    }
    Ok(resolved)
}

/// The rules a platform's file systems usually follow, as far as confining paths needs them. A
/// parameter rather than `cfg`, so the rules of each are tested on all of them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Platform {
    /// Linux and the other Unix systems: `/` separates, case matters.
    Unix,
    /// `/` separates, and APFS as macOS formats it ignores case.
    Mac,
    /// `\` and `/` separate, a path starts at a drive or a share, NTFS ignores case, and a colon
    /// after the drive selects a stream inside a file.
    Windows,
}

impl Platform {
    const HOST: Platform = if cfg!(windows) {
        Platform::Windows
    } else if cfg!(target_os = "macos") {
        Platform::Mac
    } else {
        Platform::Unix
    };

    fn separates(self, byte: u8) -> bool {
        byte == b'/' || (self == Platform::Windows && byte == b'\\')
    }

    fn ignores_case(self) -> bool {
        self != Platform::Unix
    }
}

/// One name in a path, as bytes in the platform's own encoding.
#[derive(Clone, Copy, Debug)]
struct Name<'a> {
    bytes: &'a [u8],
    /// Whether the name exists. One that exists is compared exactly: if it was read from the disk
    /// it is spelt the way the disk spells it, and a name that does not exist cannot be another
    /// spelling of it, or it would exist too. Only two names that do not exist yet are compared the
    /// way the file system will compare them once they do.
    exists: bool,
}

/// The volume a path is on, written the same way however the path spelt it, and the rest of the
/// path. On Windows `\\?\C:\x` and `C:\x` are both on `C:`, and `\\?\UNC\server\share\x` and
/// `\\server\share\x` both on `\\server\share`, so a path too long for the short form still
/// matches a root written in it. Everywhere else the volume is empty.
fn volume(path: &[u8], platform: Platform) -> (Vec<u8>, &[u8]) {
    if platform != Platform::Windows {
        return (Vec::new(), path);
    }
    let separates = |byte: Option<&u8>| byte.is_some_and(|byte| platform.separates(*byte));
    let mut rest = path;
    let verbatim = separates(rest.first())
        && separates(rest.get(1))
        && matches!(rest.get(2), Some(b'?' | b'.'))
        && separates(rest.get(3));
    let unc = if verbatim {
        rest = &rest[4..];
        let unc = rest
            .get(..3)
            .is_some_and(|start| start.eq_ignore_ascii_case(b"UNC"))
            && separates(rest.get(3));
        if unc {
            rest = &rest[4..];
        }
        unc
    } else if separates(rest.first()) && separates(rest.get(1)) {
        rest = &rest[2..];
        true
    } else {
        false
    };
    let take_name = |rest: &mut &[u8]| {
        let end = rest
            .iter()
            .position(|byte| platform.separates(*byte))
            .unwrap_or(rest.len());
        let name = rest[..end].to_ascii_lowercase();
        *rest = &rest[end..];
        name
    };
    if unc {
        let server = take_name(&mut rest);
        rest = rest.get(1..).unwrap_or_default();
        let share = take_name(&mut rest);
        let mut volume = b"\\\\".to_vec();
        volume.extend_from_slice(&server);
        volume.push(b'\\');
        volume.extend_from_slice(&share);
        (volume, rest)
    } else if rest.first().is_some_and(u8::is_ascii_alphabetic) && rest.get(1) == Some(&b':') {
        (vec![rest[0].to_ascii_uppercase(), b':'], &rest[2..])
    } else if verbatim {
        (take_name(&mut rest), rest)
    } else {
        (Vec::new(), rest)
    }
}

/// Whether a path names an alternate data stream: on Windows, a colon anywhere after its volume.
fn names_a_stream(path: &[u8], platform: Platform) -> bool {
    platform == Platform::Windows && volume(path, platform).1.contains(&b':')
}

/// The volume of a resolved path and its names, from the top down.
fn names_of(resolved: &Resolved, platform: Platform) -> (Vec<u8>, Vec<Name<'_>>) {
    let (volume, rest) = volume(resolved.existing.as_os_str().as_encoded_bytes(), platform);
    let mut names: Vec<Name<'_>> = rest
        .split(|byte| platform.separates(*byte))
        .filter(|name| !name.is_empty())
        .map(|bytes| Name {
            bytes,
            exists: true,
        })
        .collect();
    names.extend(resolved.missing.iter().map(|name| Name {
        bytes: name.as_encoded_bytes(),
        exists: false,
    }));
    if let Some(link) = &resolved.link {
        names.push(Name {
            bytes: link.as_encoded_bytes(),
            exists: true,
        });
    }
    (volume, names)
}

/// Whether `path` is `root` or lies under it.
fn within(path: &Resolved, root: &Resolved, platform: Platform) -> bool {
    let (path_volume, path_names) = names_of(path, platform);
    let (root_volume, root_names) = names_of(root, platform);
    path_volume == root_volume
        && path_names.len() >= root_names.len()
        && root_names
            .iter()
            .zip(&path_names)
            .all(|(root, name)| same_name(*root, *name, platform))
}

fn same_name(a: Name<'_>, b: Name<'_>, platform: Platform) -> bool {
    if a.bytes == b.bytes {
        return true;
    }
    if a.exists || b.exists || !platform.ignores_case() {
        return false;
    }
    // A name that is not Unicode is compared exactly: refusing a match is the safe mistake.
    match (std::str::from_utf8(a.bytes), std::str::from_utf8(b.bytes)) {
        (Ok(a), Ok(b)) => a.to_lowercase() == b.to_lowercase(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;
    use crate::config::parse;

    fn policy(text: &str) -> Policy {
        parse::<Policy>(Path::new("policy.toml"), text.as_bytes()).unwrap()
    }

    fn spec(name: &str, effect: Effect) -> ToolSpec {
        ToolSpec {
            name: name.to_owned(),
            description: String::new(),
            effect,
            input_schema: Value::Null,
        }
    }

    const DESIGN_EXAMPLE: &str = r#"
default = "owner"

[principals."agent:claude"]
role = "operator"

[principals.phone]
role = "observer"

[roles.observer]
allow = ["effect:read"]

[roles.operator]
allow = ["effect:read", "effect:write", "effect:exec"]
deny  = ["fs.remove", "power.*"]
paths = ["~/projects/**"]
"#;

    #[test]
    fn no_policy_file_means_only_the_owner_allowed_everything() {
        let empty = Policy::default();
        assert_eq!(empty.default_principal(), "owner");
        assert!(
            empty
                .authorize("owner", &spec("power.reboot", Effect::Power), None)
                .is_ok()
        );
        let error = empty
            .authorize("agent:claude", &spec("meta.version", Effect::Read), None)
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Denied);
        assert!(error.message.contains("nobody"), "{}", error.message);
    }

    #[test]
    fn rules_match_names_globs_and_effects_and_deny_wins() {
        let policy = policy(DESIGN_EXAMPLE);
        let allowed = |who: &str, name: &str, effect: Effect| {
            policy.authorize(who, &spec(name, effect), None).is_ok()
        };
        assert!(allowed("phone", "sys.info", Effect::Read));
        assert!(!allowed("phone", "fs.write", Effect::Write));
        assert!(allowed("agent:claude", "fs.write", Effect::Write));
        assert!(allowed("agent:claude", "exec.run", Effect::Exec));
        // Allowed by effect, denied by name: deny wins.
        assert!(!allowed("agent:claude", "fs.remove", Effect::Write));
        // Denied by glob, whatever the effect.
        assert!(!allowed("agent:claude", "power.targets", Effect::Read));
        assert!(!allowed("agent:claude", "fleet.add", Effect::Admin));
        let error = policy
            .authorize("agent:claude", &spec("fs.remove", Effect::Destroy), None)
            .unwrap_err();
        assert!(
            error.message.contains("rule fs.remove"),
            "{}",
            error.message
        );
    }

    #[test]
    fn a_principal_the_policy_does_not_name_is_nobody() {
        let policy = policy(DESIGN_EXAMPLE);
        let (role, _) = policy.role_of("agent:gemini");
        assert_eq!(role, "nobody");
        let error = policy
            .authorize("agent:gemini", &spec("meta.version", Effect::Read), None)
            .unwrap_err();
        assert!(
            error
                .hint
                .unwrap_or_default()
                .contains("[principals.\"agent:gemini\"]")
        );
    }

    #[test]
    fn the_owner_may_act_as_anyone_and_others_may_not() {
        let policy = policy(DESIGN_EXAMPLE);
        assert_eq!(policy.resolve(None, None).unwrap(), "owner");
        assert_eq!(
            policy.resolve(Some("agent:claude"), None).unwrap(),
            "agent:claude"
        );
        // Even a principal the policy never heard of: it is then nobody.
        assert_eq!(policy.resolve(Some("stranger"), None).unwrap(), "stranger");

        let locked = Policy {
            default: Some(Principal("phone".to_owned())),
            ..policy.clone()
        };
        assert_eq!(locked.resolve(None, None).unwrap(), "phone");
        let error = locked.resolve(Some("owner"), None).unwrap_err();
        assert_eq!(error.code, ErrorCode::Denied);
        assert_eq!(locked.resolve(Some("phone"), None).unwrap(), "phone");
    }

    #[test]
    fn a_pinned_door_ignores_a_different_as_unless_its_principal_delegates() {
        let policy = policy(DESIGN_EXAMPLE);
        assert_eq!(
            policy.resolve(Some("owner"), Some("phone")).unwrap(),
            "phone"
        );
        assert_eq!(policy.resolve(None, Some("phone")).unwrap(), "phone");
        assert_eq!(
            policy.resolve(Some("agent:claude"), Some("owner")).unwrap(),
            "agent:claude"
        );
        assert!(policy.resolve(Some("not ok"), None).is_err());
        assert!(policy.resolve(None, Some("")).is_err());
    }

    #[test]
    fn a_role_that_delegates_is_set_in_the_file() {
        let policy = policy(
            "[principals.laptop]\nrole = \"admin\"\n[roles.admin]\nallow = [\"*\"]\ndelegate = true\n",
        );
        assert_eq!(
            policy.resolve(Some("phone"), Some("laptop")).unwrap(),
            "phone"
        );
    }

    #[test]
    fn mistakes_in_the_file_are_errors_with_a_line_where_there_is_one() {
        let error = |text: &str| {
            parse::<Policy>(Path::new("policy.toml"), text.as_bytes())
                .unwrap_err()
                .message
        };
        assert!(error("[roles.x]\nallow = [\"effect:raed\"]\n").contains("line 2"));
        assert!(error("[roles.x]\nallow = [\"effect:raed\"]\n").contains("read, write"));
        assert!(error("[roles.x]\nalow = [\"fs.read\"]\n").contains("alow"));
        assert!(error("[roles.x]\nallow = [\"FS.read\"]\n").contains("line 2"));
        assert!(error("[roles.x]\npaths = [\"projects\"]\n").contains("line 2"));
        assert!(error("[roles.x]\npaths = [\"~/p*/**\"]\n").contains("wildcards"));
        assert!(
            error("[principals.\"bad name\"]\nrole = \"nobody\"\n").contains("not a principal")
        );
        assert!(error("[principals.x]\nrole = \"operatr\"\n").contains("operatr"));
        assert!(error("[roles.owner]\nallow = []\n").contains("built-in"));
        assert!(error("[principals.owner]\nrole = \"nobody\"\n").contains("built in"));
        assert!(error("default = \"phone\"\n").contains("phone"));
    }

    #[test]
    fn a_grant_reports_what_its_role_allows() {
        let policy = policy(DESIGN_EXAMPLE);
        let grant = policy.grant("agent:claude", None);
        assert_eq!(grant.role(), "operator");
        assert!(!grant.delegates());
        assert!(grant.allows(&spec("fs.read", Effect::Read)));
        assert!(!grant.allows(&spec("fs.remove", Effect::Destroy)));
        assert_eq!(grant.roots(), Some(vec!["~/projects".to_owned()]));
        assert!(Grant::owner(None).delegates());
        assert_eq!(Grant::owner(None).roots(), None);
    }

    #[test]
    fn tilde_expands_to_the_home_directory_and_only_that() {
        let home = Path::new("/home/me");
        assert_eq!(expand_tilde(Path::new("~"), Some(home)).unwrap(), home);
        assert_eq!(
            expand_tilde(Path::new("~/projects/a"), Some(home)).unwrap(),
            home.join("projects").join("a")
        );
        assert_eq!(
            expand_tilde(Path::new("/etc"), Some(home)).unwrap(),
            Path::new("/etc")
        );
        assert_eq!(
            expand_tilde(Path::new("~bob/x"), Some(home))
                .unwrap_err()
                .code,
            ErrorCode::InvalidArgs
        );
        assert_eq!(
            expand_tilde(Path::new("~/x"), None).unwrap_err().code,
            ErrorCode::Failed
        );
    }

    /// A home with a projects directory, and a grant confined to it.
    struct Confined {
        dir: tempfile::TempDir,
        grant: Grant,
    }

    impl Confined {
        fn new() -> Confined {
            let dir = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(dir.path().join("home").join("projects").join("app")).unwrap();
            std::fs::create_dir_all(dir.path().join("home").join(".ssh")).unwrap();
            std::fs::write(dir.path().join("home").join(".ssh").join("id"), "key").unwrap();
            let grant =
                policy(DESIGN_EXAMPLE).grant("agent:claude", Some(&dir.path().join("home")));
            Confined { dir, grant }
        }

        fn home(&self) -> PathBuf {
            dunce::canonicalize(self.dir.path().join("home")).unwrap()
        }

        fn check(&self, path: &str) -> Result<PathBuf, ErrorCode> {
            self.grant
                .check_path(path, Access::Read)
                .map_err(|error| error.code)
        }
    }

    #[test]
    fn paths_inside_the_roots_are_allowed_and_resolved() {
        let confined = Confined::new();
        let projects = confined.home().join("projects");
        assert_eq!(confined.check("~/projects/app"), Ok(projects.join("app")));
        assert_eq!(confined.check("~/projects"), Ok(projects.clone()));
        // Not there yet, but it will be inside once it is.
        assert_eq!(
            confined.check("~/projects/new/dir/file.txt"),
            Ok(projects.join("new").join("dir").join("file.txt"))
        );
        // Up and back down again, through directories that exist.
        assert_eq!(
            confined.check("~/projects/app/../app"),
            Ok(projects.join("app"))
        );
        let absolute = projects.join("app").to_string_lossy().into_owned();
        assert_eq!(confined.check(&absolute), Ok(projects.join("app")));
        assert!(
            confined
                .grant
                .check_path("~/projects/x", Access::Write)
                .is_ok()
        );
    }

    #[test]
    fn escapes_through_dot_dot_and_relative_paths_are_refused() {
        let confined = Confined::new();
        assert_eq!(
            confined.check("~/projects/../.ssh/id"),
            Err(ErrorCode::Denied)
        );
        assert_eq!(confined.check("~/.ssh/id"), Err(ErrorCode::Denied));
        assert_eq!(confined.check("~"), Err(ErrorCode::Denied));
        // Up out of a directory that does not exist: Unix cannot say where that leads and it is
        // refused as written; Windows folds `..` away before looking, which lands outside.
        let up_through_nothing = confined.check("~/projects/nope/../../.ssh");
        if cfg!(windows) {
            assert_eq!(up_through_nothing, Err(ErrorCode::Denied));
        } else {
            assert_eq!(up_through_nothing, Err(ErrorCode::InvalidArgs));
        }
        assert_eq!(confined.check("projects/app"), Err(ErrorCode::InvalidArgs));
        let error = confined
            .grant
            .check_path("~/.ssh/id", Access::Write)
            .unwrap_err();
        assert!(error.message.contains("may not write"), "{}", error.message);
        assert!(error.message.contains("~/projects"), "{}", error.message);
    }

    #[cfg(unix)]
    #[test]
    fn escapes_through_symbolic_links_are_refused() {
        use std::os::unix::fs::symlink;
        let confined = Confined::new();
        let home = confined.home();
        let projects = home.join("projects");
        symlink(home.join(".ssh"), projects.join("keys")).unwrap();
        symlink(home.join(".ssh").join("id"), projects.join("key")).unwrap();
        symlink(projects.join("app"), projects.join("alias")).unwrap();
        symlink(home.join("nowhere"), projects.join("dangling")).unwrap();
        // A directory link on the way out.
        assert_eq!(confined.check("~/projects/keys/id"), Err(ErrorCode::Denied));
        // A file link as the last part.
        assert_eq!(confined.check("~/projects/key"), Err(ErrorCode::Denied));
        // A link that leads nowhere cannot be checked.
        assert_eq!(
            confined.check("~/projects/dangling"),
            Err(ErrorCode::Denied)
        );
        // A link that stays inside is fine, and the last part is kept as the link itself.
        assert_eq!(
            confined.check("~/projects/alias"),
            Ok(projects.join("alias"))
        );
        assert_eq!(
            confined.check("~/projects/alias/x"),
            Ok(projects.join("app").join("x"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_dangling_link_on_the_way_is_refused_not_taken_for_a_missing_directory() {
        use std::os::unix::fs::symlink;
        let confined = Confined::new();
        let home = confined.home();
        symlink(
            home.join(".ssh").join("newdir"),
            home.join("projects").join("escape"),
        )
        .unwrap();
        let error = confined
            .grant
            .check_path("~/projects/escape/authorized_keys", Access::Write)
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Denied);
        assert!(error.message.contains("escape"), "{}", error.message);
    }

    #[cfg(unix)]
    #[test]
    fn a_last_part_that_cannot_be_looked_at_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let confined = Confined::new();
        let locked = confined.home().join("projects").join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o600)).unwrap();
        // Root may look anyway, and then there is nothing to test.
        let refused = std::fs::symlink_metadata(locked.join("x"))
            .is_err_and(|error| error.kind() == io::ErrorKind::PermissionDenied);
        let result = confined.check("~/projects/locked/x");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
        if refused {
            assert_eq!(result, Err(ErrorCode::Denied));
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_root_that_is_itself_a_link_confines_to_where_it_leads() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("data").join("projects");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::create_dir_all(dir.path().join("home")).unwrap();
        symlink(&real, dir.path().join("home").join("projects")).unwrap();
        let grant = policy(DESIGN_EXAMPLE).grant("agent:claude", Some(&dir.path().join("home")));
        let resolved = grant.check_path("~/projects/a.txt", Access::Write).unwrap();
        assert_eq!(resolved, dunce::canonicalize(&real).unwrap().join("a.txt"));
    }

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn another_spelling_of_a_root_on_a_disk_that_ignores_case_is_the_root() {
        let confined = Confined::new();
        let projects = confined.home().join("projects");
        assert_eq!(confined.check("~/PROJECTS"), Ok(projects.clone()));
        assert_eq!(
            confined.check("~/Projects/App/new.txt"),
            Ok(projects.join("app").join("new.txt"))
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_refuses_streams_and_matches_paths_too_long_for_the_short_form() {
        let confined = Confined::new();
        assert_eq!(
            confined.check("~/projects/app::$DATA"),
            Err(ErrorCode::InvalidArgs)
        );
        let mut deep = confined.home().join("projects");
        for n in 0..30 {
            deep.push(format!("directory{n:02}"));
        }
        std::fs::create_dir_all(&deep).unwrap();
        let long = deep.join("file.txt");
        assert!(long.as_os_str().len() > 260);
        let checked = confined.grant.check_path(&long, Access::Write).unwrap();
        assert!(checked.ends_with(Path::new("directory29").join("file.txt")));
    }

    fn resolved(existing: &str, missing: &[&str]) -> Resolved {
        Resolved {
            existing: PathBuf::from(existing),
            missing: missing.iter().map(OsString::from).collect(),
            link: None,
        }
    }

    #[test]
    fn windows_paths_match_their_roots_however_the_volume_is_spelt() {
        let root = resolved(r"C:\Users\me\projects", &[]);
        let inside = |path: &str| within(&resolved(path, &[]), &root, Platform::Windows);
        let long = format!(r"\\?\C:\Users\me\projects\{}", "a".repeat(300));
        assert!(inside(&long));
        assert!(inside(r"c:\Users\me\projects\x"));
        assert!(inside("C:/Users/me/projects"));
        assert!(!inside(r"D:\Users\me\projects"));
        assert!(!inside(r"C:\Users\me\projects2"));
        assert!(!inside(r"C:\Users\me"));

        let share = resolved(r"\\tower\Data\projects", &[]);
        let on_share = |path: &str| within(&resolved(path, &[]), &share, Platform::Windows);
        assert!(on_share(r"\\?\UNC\TOWER\data\projects\x"));
        assert!(!on_share(r"\\pi\data\projects"));
        assert!(!on_share(r"C:\tower\data\projects"));

        // Elsewhere a backslash is part of a name, and the long form is not the short one.
        assert!(!within(&resolved(&long, &[]), &root, Platform::Unix));
    }

    #[test]
    fn case_is_ignored_only_between_names_that_do_not_exist_yet_and_only_where_disks_ignore_it() {
        let root = resolved("/Users/me", &["projects"]);
        let path = resolved("/Users/me", &["PROJECTS", "x"]);
        assert!(within(&path, &root, Platform::Mac));
        assert!(!within(&path, &root, Platform::Unix));
        assert!(within(
            &resolved(r"C:\Users\me", &["Projects", "x"]),
            &resolved(r"C:\Users\me", &["projects"]),
            Platform::Windows
        ));
        // Names read from the disk are spelt the way the disk spells them, so two spellings are
        // two entries, as they are on a volume that minds case.
        let existing = resolved("/Users/me/projects", &[]);
        assert!(!within(
            &resolved("/Users/me/PROJECTS/x", &[]),
            &existing,
            Platform::Mac
        ));
        // And a name that does not exist is no other spelling of one that does, or it would exist.
        assert!(!within(
            &resolved("/Users/me", &["PROJECTS", "x"]),
            &existing,
            Platform::Mac
        ));
        assert!(within(
            &resolved("/Users/me/projects", &["new", "x"]),
            &existing,
            Platform::Mac
        ));
    }

    #[test]
    fn only_windows_has_streams_and_only_after_the_volume() {
        assert!(names_a_stream(
            br"C:\Users\me\key::$DATA",
            Platform::Windows
        ));
        assert!(names_a_stream(
            br"C:\Users\me\notes.txt:hidden",
            Platform::Windows
        ));
        assert!(names_a_stream(br"\\?\C:\Users\me\key:x", Platform::Windows));
        assert!(names_a_stream(br"\\tower\share\key:x", Platform::Windows));
        assert!(!names_a_stream(br"C:\Users\me\key", Platform::Windows));
        assert!(!names_a_stream(br"\\?\C:\Users\me\key", Platform::Windows));
        assert!(!names_a_stream(b"/home/me/a:b", Platform::Unix));
        assert!(!names_a_stream(b"/Users/me/a:b", Platform::Mac));
    }

    #[test]
    fn an_unconfined_grant_still_resolves_and_refuses_relative_paths() {
        let dir = tempfile::tempdir().unwrap();
        let grant = Grant::owner(Some(dir.path()));
        let resolved = grant
            .check_path("~/anything/at/all", Access::Write)
            .unwrap();
        assert_eq!(
            resolved,
            dunce::canonicalize(dir.path())
                .unwrap()
                .join("anything")
                .join("at")
                .join("all")
        );
        assert_eq!(
            grant.check_path("relative", Access::Read).unwrap_err().code,
            ErrorCode::InvalidArgs
        );
    }
}
