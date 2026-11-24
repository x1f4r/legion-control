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
//!
//! Whatever its `paths`, a role keeps its file tools out of the node's own configuration and state
//! unless it allows `effect:admin`: a role that may write files could otherwise rewrite its own
//! policy, or the audit log of what it did.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::io;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, OnceLock};

use legion_proto::{Effect, ErrorBody, ErrorCode, ToolSpec};
use same_file::Handle;
use serde::Deserialize;
use serde_json::json;

use crate::config::Document;
use crate::glob;
use crate::paths::Paths;

/// The principal a local shell is, and the role that may do anything.
pub const OWNER: &str = "owner";
/// The role that may do nothing, given to every principal the policy does not name.
pub const NOBODY: &str = "nobody";

/// Well-known secret locations relative to the home directory (or Windows user profile).
///
/// File tools refuse reading or writing any of these — and recursive walks skip them — for every
/// role without `effect:admin`, unless that role's `paths` explicitly names the location or a path
/// inside it (a broad root such as `~/**` does not count). This is a guard rail for file tools; it
/// does not bind a role that allows `effect:exec`.
///
/// Deliberately NOT covered: `.npmrc` and shell history files (`.bash_history`, `.zsh_history`,
/// fish/PowerShell history), because they cause too many false positives in normal development.
/// Owners who want to protect those or any other custom path can add top-level
/// `secrets = ["~/extra/path"]` in `policy.toml`, which is enforced identically to the built-in
/// list.
///
/// | Path (relative to `~`) | What lives there |
/// | --- | --- |
/// | `.ssh` | SSH private keys and client configuration |
/// | `.gnupg` | GPG private keyrings |
/// | `.aws` | AWS credentials and configuration |
/// | `.azure` | Azure CLI tokens and credentials |
/// | `.kube` | Kubernetes client certificates and tokens |
/// | `.docker/config.json` | Container registry credentials |
/// | `.netrc` | Plaintext remote login credentials |
/// | `.git-credentials` | Git credential-store tokens |
/// | `.config/gh` | GitHub CLI authentication tokens |
/// | `.config/gcloud` | Google Cloud SDK credentials |
/// | `.config/rclone` | rclone remote storage credentials |
/// | `.config/op` | 1Password CLI session and configuration |
/// | `.vault-token` | HashiCorp Vault authentication token |
/// | `.terraform.d/credentials.tfrc.json` | Terraform CLI registry tokens |
/// | `.cargo/credentials.toml` | Cargo registry tokens |
/// | `.pypirc` | PyPI upload tokens and credentials |
/// | `.password-store` | `pass` encrypted password store |
/// | `.local/share/keyrings` | Linux desktop keyrings |
/// | `Library/Keychains` | macOS Keychain databases |
/// | `.claude.json` | Claude Code account state and OAuth metadata |
/// | `.claude/.credentials.json` | Claude Code OAuth credentials |
/// | `.codex/auth.json` | Codex CLI authentication tokens |
/// | `.gemini/oauth_creds.json` | Gemini CLI OAuth credentials |
/// | `.gemini/google_accounts.json` | Gemini CLI account credentials |
/// | `.config/opencode/service.json` | OpenCode local service credentials |
/// | `.local/share/opencode/auth.json` | OpenCode authentication tokens |
/// | `.config/github-copilot` | GitHub Copilot CLI tokens |
/// | `.mozilla` | Firefox browser profiles (Linux) |
/// | `.thunderbird` | Thunderbird mail profiles and credentials |
/// | `.config/google-chrome` | Google Chrome profiles (Linux) |
/// | `.config/chromium` | Chromium profiles (Linux) |
/// | `.config/BraveSoftware/Brave-Browser` | Brave browser profiles (Linux) |
/// | `.config/microsoft-edge` | Microsoft Edge profiles (Linux) |
/// | `Library/Application Support/Google/Chrome` | Google Chrome profiles (macOS) |
/// | `Library/Application Support/Firefox` | Firefox profiles (macOS) |
/// | `Library/Application Support/BraveSoftware/Brave-Browser` | Brave browser profiles (macOS) |
/// | `Library/Application Support/Microsoft Edge` | Microsoft Edge profiles (macOS) |
/// | `Library/Cookies` | Safari and WebKit cookies (macOS) |
/// | `Library/Containers/com.apple.Safari/Data/Library/Cookies` | Safari sandboxed cookies (macOS) |
/// | `AppData/Local/Google/Chrome/User Data` | Google Chrome profiles (Windows) |
/// | `AppData/Local/Microsoft/Edge/User Data` | Microsoft Edge profiles (Windows) |
/// | `AppData/Local/BraveSoftware/Brave-Browser/User Data` | Brave browser profiles (Windows) |
/// | `AppData/Roaming/Mozilla/Firefox` | Firefox profiles (Windows) |
/// | `AppData/Roaming/Microsoft/Credentials` | Windows Credential Manager roaming store |
/// | `AppData/Roaming/Microsoft/Protect` | Windows DPAPI master keys |
/// | `AppData/Local/Microsoft/Credentials` | Windows Credential Manager local store |
pub const SECRET_LOCATIONS: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".aws",
    ".azure",
    ".kube",
    ".docker/config.json",
    ".netrc",
    ".git-credentials",
    ".config/gh",
    ".config/gcloud",
    ".config/rclone",
    ".config/op",
    ".vault-token",
    ".terraform.d/credentials.tfrc.json",
    ".cargo/credentials.toml",
    ".pypirc",
    ".password-store",
    ".local/share/keyrings",
    "Library/Keychains",
    ".claude.json",
    ".claude/.credentials.json",
    ".codex/auth.json",
    ".gemini/oauth_creds.json",
    ".gemini/google_accounts.json",
    ".config/opencode/service.json",
    ".local/share/opencode/auth.json",
    ".config/github-copilot",
    ".mozilla",
    ".thunderbird",
    ".config/google-chrome",
    ".config/chromium",
    ".config/BraveSoftware/Brave-Browser",
    ".config/microsoft-edge",
    "Library/Application Support/Google/Chrome",
    "Library/Application Support/Firefox",
    "Library/Application Support/BraveSoftware/Brave-Browser",
    "Library/Application Support/Microsoft Edge",
    "Library/Cookies",
    "Library/Containers/com.apple.Safari/Data/Library/Cookies",
    "AppData/Local/Google/Chrome/User Data",
    "AppData/Local/Microsoft/Edge/User Data",
    "AppData/Local/BraveSoftware/Brave-Browser/User Data",
    "AppData/Roaming/Mozilla/Firefox",
    "AppData/Roaming/Microsoft/Credentials",
    "AppData/Roaming/Microsoft/Protect",
    "AppData/Local/Microsoft/Credentials",
];

/// `policy.toml`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    /// The principal of a caller who names none; `owner` when absent.
    #[serde(default)]
    pub default: Option<Principal>,
    /// Extra secret locations (absolute or starting with `~`) treated identically to
    /// [`SECRET_LOCATIONS`] for every role without `effect:admin`.
    #[serde(default)]
    pub secrets: Vec<PathRoot>,
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
    /// Whether a caller with this role may act as another principal. Only the built-in owner role
    /// does: a role that could act as anyone would be the owner under another name, so a file
    /// that sets it on a role of its own is refused.
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
        if let Some((name, _)) = self.roles.iter().find(|(_, role)| role.delegate) {
            return Err(format!(
                "[roles.{name}] sets delegate = true, but acting as another principal is the owner's alone: a role that could act as anyone would be the owner under another name; give the principal role = \"owner\" if that is what is meant"
            ));
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
            .with_hint("Call without --as: only the owner role may act as another principal."))
        }
    }

    /// Whether `principal` may call the tool, and if so the grant its call carries. `paths` are
    /// the node's: the grant expands `~` to its home and keeps file tools out of its configuration
    /// and state.
    pub fn authorize(
        &self,
        principal: &str,
        spec: &ToolSpec,
        paths: &Paths,
    ) -> Result<Grant, ErrorBody> {
        self.authorize_name(principal, &spec.name, Some(spec.effect), paths)
    }

    /// Whether `principal` may call `tool`, given its `effect` when known.
    ///
    /// When `effect` is `Some`, this behaves identically to [`Policy::authorize`]. When `effect`
    /// is `None` (a tool that exists only on another node whose effect could not be determined),
    /// any matching `deny` rule or any `deny` rule over an effect refuses the call, and only a
    /// wildcard `*` rule in `allow` permits it.
    pub fn authorize_name(
        &self,
        principal: &str,
        tool: &str,
        effect: Option<Effect>,
        paths: &Paths,
    ) -> Result<Grant, ErrorBody> {
        let grant = self.grant(principal, paths);
        let role = &grant.role;
        if let Some(effect) = effect {
            let matches_rule = |rule: &Rule| match rule {
                Rule::Effect(of) => *of == effect,
                Rule::Tools(pattern) => glob::matches(pattern, tool),
            };
            if let Some(rule) = grant.rules.deny.iter().find(|rule| matches_rule(rule)) {
                return Err(ErrorBody::new(
                    ErrorCode::Denied,
                    format!(
                        "{principal} may not use {tool}: its role {role} denies it with the rule {rule}."
                    ),
                )
                .with_hint(
                    "Ask the owner of this machine to change policy.toml if it should be allowed.",
                )
                .with_details(json!({"principal": principal, "role": role, "tool": tool, "rule": rule.to_string()})));
            }
            if grant.rules.allow.iter().any(matches_rule) {
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
                        "{principal} may not use {tool}: its role {role} allows no rule that matches it (its effect is {effect})."
                    ),
                    "Ask the owner of this machine to change policy.toml if it should be allowed."
                        .to_owned(),
                )
            };
            return Err(ErrorBody::new(ErrorCode::Denied, message)
                .with_hint(hint)
                .with_details(json!({"principal": principal, "role": role, "tool": tool})));
        }

        // Effect unknown (extension tool not registered locally and not reported by the target).
        if let Some(rule) = grant.rules.deny.iter().find(|rule| match rule {
            Rule::Tools(pattern) => glob::matches(pattern, tool),
            Rule::Effect(_) => true,
        }) {
            return Err(ErrorBody::new(
                ErrorCode::Denied,
                format!(
                    "{principal} may not use {tool}: its role {role} denies it (rule {rule}) and its effect is not known here."
                ),
            )
            .with_hint("Ask the owner of this machine to change policy.toml if it should be allowed.")
            .with_details(json!({"principal": principal, "role": role, "tool": tool, "rule": rule.to_string()})));
        }
        let allows_all =
            grant.rules.allow.iter().any(
                |rule| matches!(rule, Rule::Tools(pattern) if pattern.chars().all(|c| c == '*')),
            );
        if allows_all {
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
                    "{principal} may not use {tool}: its effect is not known on this node and role {role} does not allow `*`."
                ),
                "Ask the owner of this machine to change policy.toml if it should be allowed."
                    .to_owned(),
            )
        };
        Err(ErrorBody::new(ErrorCode::Denied, message)
            .with_hint(hint)
            .with_details(json!({"principal": principal, "role": role, "tool": tool})))
    }

    /// Whether `principal` might be allowed to call `tool` once the target reports its effect.
    ///
    /// Returns `false` when `tool` is already denied by name/glob or when no allow rule could
    /// match it for any effect, so a forwarding node can fail closed without contacting the target.
    pub fn could_allow_with_remote_effect(&self, principal: &str, tool: &str) -> bool {
        let (_, rules) = self.role_of(principal);
        let denied_by_name = rules
            .deny
            .iter()
            .any(|rule| matches!(rule, Rule::Tools(pattern) if glob::matches(pattern, tool)));
        if denied_by_name {
            return false;
        }
        rules.allow.iter().any(|rule| match rule {
            Rule::Effect(_) => true,
            Rule::Tools(pattern) => glob::matches(pattern, tool),
        })
    }

    /// The grant a principal has, whatever it calls; what `meta.whoami` reports.
    pub fn grant(&self, principal: &str, paths: &Paths) -> Grant {
        let (role, rules) = self.role_of(principal);
        Grant {
            principal: principal.to_owned(),
            role,
            rules: Arc::new(rules),
            extra_secrets: Arc::from(self.secrets.clone()),
            paths: paths.clone(),
            protected_cache: Arc::new(OnceLock::new()),
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
    extra_secrets: Arc<[PathRoot]>,
    paths: Paths,
    protected_cache: Arc<OnceLock<CachedProtected>>,
}

/// Protected directories, secret locations, and configured roots resolved once for the lifetime of
/// a [`Grant`] so repeated `check_path` and directory-walk checks avoid re-resolving dozens of
/// paths on every call.
#[derive(Debug)]
struct CachedProtected {
    own_dirs: Vec<Result<OwnDir, (Place, PathBuf, Unresolved)>>,
    secrets: Vec<SecretPlace>,
    secret_prefixes: Vec<SecretPrefix>,
    resolved_roots: Vec<Resolved>,
}

/// Whether a path is wanted for reading or for writing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// The tool will read it, list it or look at it.
    Read,
    /// The tool will create, change, move or remove it. One that removes, moves or replaces
    /// everything under it asks [`Grant::check_tree`] as well.
    Write,
}

/// Kind of filesystem entry a tool is about to create, replace, or link at a path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CreateKind {
    /// A regular file.
    File,
    /// A directory.
    Dir,
    /// A symbolic link (regardless of what it points to).
    Symlink,
}

/// A place a tool that acts on a whole tree treats with care, as [`Grant::protected`] names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Place {
    /// The root of a file system, such as `/` or `C:\`.
    Root,
    /// The home directory `~` stands for.
    Home,
    /// The node's configuration directory.
    Config,
    /// The node's state directory: the audit log and, later, jobs.
    State,
    /// A well-known secret location under the home directory (see [`SECRET_LOCATIONS`]).
    Secret,
}

impl Place {
    fn describe(self) -> &'static str {
        match self {
            Place::Root => "the root of a file system",
            Place::Home => "the home directory",
            Place::Config => "Legion's own configuration",
            Place::State => "Legion's own state",
            Place::Secret => "a secret location",
        }
    }
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
    pub fn owner(paths: &Paths) -> Grant {
        Policy::default().grant(OWNER, paths)
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

    /// Whether this grant's role may use tools of this effect at all, decided as authorising a call
    /// decides: a rule for the effect, or `*`, which covers every tool, allows it, and the same in
    /// `deny` forbids it. A rule that names some tools says nothing about the effect as a whole.
    pub fn allows_effect(&self, effect: Effect) -> bool {
        let covers = |rule: &Rule| match rule {
            Rule::Effect(of) => *of == effect,
            Rule::Tools(pattern) => pattern.chars().all(|c| c == '*'),
        };
        !self.rules.deny.iter().any(covers) && self.rules.allow.iter().any(covers)
    }

    /// The directories file tools are confined to, as written in the policy (`~` not expanded), or
    /// `None` when they are not confined. A tool that needs a working directory nobody named takes
    /// the first of them, through [`Grant::check_path`], and the home directory when there are none.
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
        let expanded = expand_tilde(given, self.paths.home())?;
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
        validate_platform_path(
            expanded.as_os_str().as_encoded_bytes(),
            given,
            Platform::HOST,
        )?;

        let Some(roots) = &self.rules.paths else {
            let resolved =
                resolve_keeping_last(&expanded).map_err(|error| error.unconfined(given))?;
            let resolved_path = resolved.path();
            validate_platform_path(
                resolved_path.as_os_str().as_encoded_bytes(),
                given,
                Platform::HOST,
            )?;
            self.keep_out_of_own_dirs(&resolved, given, access)?;
            self.keep_out_of_secret_locations(&resolved, given, access, &[])?;
            return Ok(resolved_path);
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
            let expanded = expand_tilde(Path::new(root.as_str()), self.paths.home())?;
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
        let resolved_path = resolved.path();
        validate_platform_path(
            resolved_path.as_os_str().as_encoded_bytes(),
            given,
            Platform::HOST,
        )?;
        self.keep_out_of_own_dirs(&resolved, given, access)?;
        self.keep_out_of_secret_locations(&resolved, given, access, &resolved_roots)?;
        Ok(resolved_path)
    }

    /// Whether a path is one of the places a tool must not remove, move or replace as a whole, or a
    /// directory that holds one: the root of a file system, the home directory, the node's
    /// configuration or state directory, or (for roles without `effect:admin` that did not opt in
    /// through `paths`) a well-known secret location, named in that order of precedence.
    ///
    /// `path` is one [`Grant::check_path`] returned. A last part that is a symbolic link is never
    /// one of them, since acting on it acts on the link. Names are compared as the file system
    /// compares them and, where both exist, so are the files themselves, so neither a symbolic link
    /// nor another spelling hides a place. [`Grant::check_tree`] says what to refuse.
    pub fn protected(&self, path: &Path) -> Result<Option<Place>, ErrorBody> {
        let unclear = |error: Unresolved| match error {
            Unresolved::Invalid(error) => error,
            Unresolved::Unclear(code, reason) => ErrorBody::new(
                code,
                format!("{} cannot be checked: {reason}.", path.display()),
            ),
        };
        let resolved = resolve_keeping_last(path).map_err(unclear)?;
        if resolved.link.is_some() {
            return Ok(None);
        }
        if names_of(&resolved, Platform::HOST).1.is_empty() {
            return Ok(Some(Place::Root));
        }
        // Only a path that exists has an identity to find among a place's ancestors.
        let identity = if resolved.missing.is_empty() {
            Handle::from_path(&resolved.existing).ok()
        } else {
            None
        };
        let holds = |place: &Resolved| {
            within(place, &resolved, Platform::HOST)
                || identity.as_ref().is_some_and(|identity| {
                    place
                        .existing
                        .ancestors()
                        .any(|ancestor| Handle::from_path(ancestor).is_ok_and(|h| h == *identity))
                })
        };
        if let Some(home) = self.paths.home() {
            let home = resolve(home).map_err(unclear)?;
            if holds(&home) {
                return Ok(Some(Place::Home));
            }
        }
        let cache = self.protected_cache();
        for entry in &cache.own_dirs {
            let dir = entry
                .as_ref()
                .map_err(|(_, _, error)| unclear(error.clone()))?;
            if holds(&dir.resolved) {
                return Ok(Some(dir.place));
            }
        }
        if !self.allows_effect(Effect::Admin) {
            for secret in &cache.secrets {
                if secret_explicitly_opted_in_whole(secret, &cache.resolved_roots) {
                    continue;
                }
                if holds(&secret.resolved) {
                    return Ok(Some(Place::Secret));
                }
            }
        }
        Ok(None)
    }

    /// Checks a path a tool is about to remove, move or replace with everything under it, after
    /// [`Grant::check_path`] has: the root of a file system and the home directory are refused to
    /// every role, a directory holding the node's configuration or state to a role that does not
    /// allow `effect:admin`, and a secret location (or an ancestor holding one) to a role without
    /// `effect:admin` that did not explicitly opt in via `paths`.
    pub fn check_tree(&self, path: &Path) -> Result<(), ErrorBody> {
        let place = match self.protected(path)? {
            None => return Ok(()),
            Some(Place::Config | Place::State | Place::Secret)
                if self.allows_effect(Effect::Admin) =>
            {
                return Ok(());
            }
            Some(place) => place,
        };
        let hint = match place {
            Place::Root | Place::Home => {
                "Name what is under it instead; no tool takes all of it at once."
            }
            Place::Config | Place::State => {
                "Only a role that allows effect:admin may change Legion's own files; ask the owner of this machine."
            }
            Place::Secret => {
                "Only a role that allows effect:admin, or one whose `paths` names the secret location explicitly, may remove, move or replace it."
            }
        };
        Err(ErrorBody::new(
            ErrorCode::Denied,
            format!(
                "{} may not remove, move or replace all of {}: it is or holds {}.",
                self.principal,
                path.display(),
                place.describe()
            ),
        )
        .with_hint(hint)
        .with_details(json!({"path": path, "place": format!("{place:?}").to_lowercase()})))
    }

    /// Refuses creating, replacing, or linking any path that is — or holds — Legion's own
    /// configuration/state directories or a non-opted-in secret location, for roles without
    /// `effect:admin`.
    ///
    /// Unlike [`Grant::protected`], this never exempts a symbolic link at the target path: a
    /// symlink may never be created at a secret location or any proper ancestor of one, whatever
    /// it points to. When `create_parents` is `true`, every missing parent directory that would be
    /// created is checked with the same rule.
    pub fn check_create(
        &self,
        path: &Path,
        kind: CreateKind,
        create_parents: bool,
    ) -> Result<(), ErrorBody> {
        if self.allows_effect(Effect::Admin) {
            return Ok(());
        }
        let unclear = |error: Unresolved| match error {
            Unresolved::Invalid(error) => error,
            Unresolved::Unclear(code, reason) => ErrorBody::new(
                code,
                format!("{} cannot be checked: {reason}.", path.display()),
            ),
        };
        let resolved = resolve_keeping_last(path).map_err(unclear)?;
        let cache = self.protected_cache();
        let own: Vec<&OwnDir> = cache
            .own_dirs
            .iter()
            .map(|entry| {
                entry.as_ref().map_err(|(_, dir, error)| {
                    let reason = match error {
                        Unresolved::Invalid(error) => error.message.clone(),
                        Unresolved::Unclear(_, reason) => reason.clone(),
                    };
                    ErrorBody::new(
                        ErrorCode::Denied,
                        format!(
                            "{} may not create or replace {}: Legion's own directory {} cannot be resolved ({reason}).",
                            self.principal,
                            path.display(),
                            dir.display()
                        ),
                    )
                    .with_hint("Run meta.doctor as the owner to see what is wrong with it.")
                })
            })
            .collect::<Result<_, _>>()?;
        let secrets: Vec<&SecretPlace> = cache
            .secrets
            .iter()
            .filter(|secret| !secret_explicitly_opted_in_whole(secret, &cache.resolved_roots))
            .collect();
        let home_resolved = self.paths.home().and_then(|home| resolve(home).ok());

        let check_one = |candidate: &Resolved,
                         candidate_kind: CreateKind,
                         is_parent: bool|
         -> Result<(), ErrorBody> {
            let identity = if candidate.missing.is_empty() && candidate.link.is_none() {
                Handle::from_path(&candidate.existing).ok()
            } else {
                None
            };
            let holds = |place: &Resolved| {
                within_protected(place, candidate, Platform::HOST)
                    || identity.as_ref().is_some_and(|id| {
                        place
                            .existing
                            .ancestors()
                            .any(|anc| Handle::from_path(anc).is_ok_and(|h| h == *id))
                    })
            };
            let at_or_above_home = home_resolved.as_ref().is_some_and(&holds);
            let exempt_existing_home_dir = candidate_kind != CreateKind::Symlink
                && at_or_above_home
                && candidate.missing.is_empty()
                && candidate.link.is_none();

            for dir in &own {
                let touches = within_protected(candidate, &dir.resolved, Platform::HOST)
                    || holds(&dir.resolved);
                if touches && !exempt_existing_home_dir {
                    let what = if is_parent {
                        format!(
                            "creating missing parent {} would create an ancestor of {}",
                            candidate.path().display(),
                            dir.place.describe()
                        )
                    } else {
                        format!("it is or holds {}", dir.place.describe())
                    };
                    return Err(ErrorBody::new(
                        ErrorCode::Denied,
                        format!(
                            "{} may not create or replace {}: {what}.",
                            self.principal,
                            path.display()
                        ),
                    )
                    .with_hint("Only a role that allows effect:admin may change Legion's own files or their ancestors.")
                    .with_details(json!({
                        "path": path,
                        "place": format!("{:?}", dir.place).to_lowercase(),
                    })));
                }
            }

            for secret in &secrets {
                let touches = within_protected(candidate, &secret.resolved, Platform::HOST)
                    || holds(&secret.resolved);
                if touches && !exempt_existing_home_dir {
                    let what = if candidate_kind == CreateKind::Symlink {
                        format!(
                            "a symbolic link may not be placed at or above secret location {}",
                            secret.label
                        )
                    } else if is_parent {
                        format!(
                            "creating missing parent {} would create an ancestor of secret location {}",
                            candidate.path().display(),
                            secret.label
                        )
                    } else {
                        format!("it is or holds secret location {}", secret.label)
                    };
                    return Err(ErrorBody::new(
                        ErrorCode::Denied,
                        format!(
                            "{} may not create or replace {}: {what}.",
                            self.principal,
                            path.display()
                        ),
                    )
                    .with_hint(format!(
                        "Only roles with effect:admin, or roles whose `paths` explicitly names {}, may create or replace it or its missing parents.",
                        secret.label
                    ))
                    .with_details(json!({
                        "path": path,
                        "place": "secret",
                        "secret": secret.label,
                    })));
                }
            }
            for prefix in &cache.secret_prefixes {
                if prefix.matches_resolved(candidate) {
                    return Err(ErrorBody::new(
                        ErrorCode::Denied,
                        format!(
                            "{} may not create or replace {}: it is a staging file for {}.",
                            self.principal,
                            path.display(),
                            prefix.label
                        ),
                    )
                    .with_hint(format!(
                        "Only roles with effect:admin may touch atomic-write staging files beside {}.",
                        prefix.label
                    ))
                    .with_details(json!({
                        "path": path,
                        "place": "secret",
                        "secret": prefix.label,
                    })));
                }
            }
            Ok(())
        };

        check_one(&resolved, kind, false)?;

        if create_parents && resolved.missing.len() > 1 {
            for prefix_len in 1..resolved.missing.len() {
                let parent_resolved = Resolved {
                    existing: resolved.existing.clone(),
                    missing: resolved.missing[..prefix_len].to_vec(),
                    link: None,
                };
                check_one(&parent_resolved, CreateKind::Dir, true)?;
            }
        }

        Ok(())
    }

    /// Resolves and caches Legion's own directories, secret locations, and configured roots once
    /// for the lifetime of this [`Grant`].
    fn protected_cache(&self) -> &CachedProtected {
        self.protected_cache.get_or_init(|| {
            let (secrets, secret_prefixes) = self.compute_secret_locations();
            CachedProtected {
                own_dirs: self.compute_own_dirs(),
                secrets,
                secret_prefixes,
                resolved_roots: self.resolved_roots_best_effort(),
            }
        })
    }

    /// Pre-resolved filter over Legion's own directories and well-known secret locations for
    /// recursive directory walks (`fs.list`, `fs.find`, `fs.grep`, `fs.copy`, `fs.manifest`).
    ///
    /// Returns an empty filter (blocks nothing) for roles that allow `effect:admin`.
    /// If Legion's own configuration or state directory cannot be resolved, keeps a raw-path
    /// fallback entry and fails closed so walk entries are never treated as safe when in doubt.
    pub fn protected_filter(&self) -> ProtectedFilter {
        if self.allows_effect(Effect::Admin) {
            return ProtectedFilter::allow_all();
        }
        let cache = self.protected_cache();
        let mut entries = Vec::new();
        let mut fail_closed_reason: Option<(Place, String)> = None;
        for entry in &cache.own_dirs {
            match entry {
                Ok(dir) => {
                    let reason = format!(
                        "{}, which only a role that allows effect:admin may read or change",
                        dir.place.describe()
                    );
                    entries.push(ProtectedEntry {
                        place: dir.place,
                        reason,
                        raw_path: dir.dir.clone(),
                        resolved: dir.resolved.clone(),
                        identity: dir.identity.clone(),
                        #[cfg(unix)]
                        dev_ino: dir.dev_ino,
                    });
                }
                Err((place, dir, error)) => {
                    let reason = format!(
                        "{}, which only a role that allows effect:admin may read or change",
                        place.describe()
                    );
                    let detail = match error {
                        Unresolved::Invalid(err) => err.message.as_str(),
                        Unresolved::Unclear(_, msg) => msg.as_str(),
                    };
                    entries.push(ProtectedEntry {
                        place: *place,
                        reason,
                        raw_path: dir.clone(),
                        resolved: raw_fallback_resolved(dir),
                        identity: None,
                        #[cfg(unix)]
                        dev_ino: None,
                    });
                    if fail_closed_reason.is_none() {
                        fail_closed_reason = Some((
                            *place,
                            format!(
                                "{} ({}) could not be resolved ({detail}), so paths cannot be told apart from it",
                                place.describe(),
                                dir.display()
                            ),
                        ));
                    }
                }
            }
        }
        for secret in &cache.secrets {
            if secret_explicitly_opted_in_whole(secret, &cache.resolved_roots) {
                continue;
            }
            let reason = format!(
                "{} is on Legion's list of secret locations; name it explicitly in the role's `paths` to allow access",
                secret.label
            );
            entries.push(ProtectedEntry {
                place: Place::Secret,
                reason,
                raw_path: secret.raw_path.clone(),
                resolved: secret.resolved.clone(),
                identity: secret.identity.clone(),
                #[cfg(unix)]
                dev_ino: secret.dev_ino,
            });
        }
        let explicit_roots = cache
            .resolved_roots
            .iter()
            .map(|root| {
                let id = if root.missing.is_empty() {
                    Handle::from_path(&root.existing).ok().map(Arc::new)
                } else {
                    None
                };
                #[cfg(unix)]
                let dev_ino = if root.missing.is_empty() {
                    std::fs::symlink_metadata(&root.existing)
                        .ok()
                        .map(|m| (m.dev(), m.ino()))
                } else {
                    None
                };
                ExplicitRoot {
                    resolved: root.clone(),
                    identity: id,
                    #[cfg(unix)]
                    dev_ino,
                }
            })
            .collect();
        let has_identities = entries.iter().any(|entry| entry.identity.is_some());
        ProtectedFilter {
            entries,
            secret_prefixes: cache.secret_prefixes.clone(),
            explicit_roots,
            has_identities,
            fail_closed_reason,
        }
    }

    /// Best-effort resolution of this role's configured `paths` roots.
    fn resolved_roots_best_effort(&self) -> Vec<Resolved> {
        let Some(roots) = &self.rules.paths else {
            return Vec::new();
        };
        roots
            .iter()
            .filter_map(|root| {
                let expanded = expand_tilde(Path::new(root.as_str()), self.paths.home()).ok()?;
                resolve(&expanded).ok()
            })
            .collect()
    }

    /// Well-known secret locations under `self.paths.home()` plus any top-level `secrets` from
    /// `policy.toml`, resolved as far as they exist, along with sibling staging-file prefixes
    /// (`.<name>.legion-tmp-*`) beside active `authorized_keys` files.
    ///
    /// A secret location that cannot be resolved (for example a dangling symlink or an unreadable
    /// parent) is never silently dropped: it falls back to `resolve_keeping_last` and then to a
    /// lexical raw path so both `check_path` and directory walks still block it.
    fn compute_secret_locations(&self) -> (Vec<SecretPlace>, Vec<SecretPrefix>) {
        let mut places = Vec::with_capacity(SECRET_LOCATIONS.len() + self.extra_secrets.len() + 6);
        let mut prefixes = Vec::new();
        let home_resolved = self.paths.home().and_then(|home| resolve(home).ok());
        if let Some(home) = self.paths.home() {
            for rel in SECRET_LOCATIONS {
                let full = rel
                    .split('/')
                    .fold(home.to_path_buf(), |acc, part| acc.join(part));
                let resolved = resolve(&full)
                    .or_else(|_| resolve_keeping_last(&full))
                    .unwrap_or_else(|_| match &home_resolved {
                        Some(base) => {
                            let mut missing = base.missing.clone();
                            missing.extend(rel.split('/').map(OsString::from));
                            Resolved {
                                existing: base.existing.clone(),
                                missing,
                                link: None,
                            }
                        }
                        None => raw_fallback_resolved(&full),
                    });
                places.push(build_secret_place(format!("~/{rel}"), full, resolved));
            }
            // If `~/.ssh/authorized_keys` is a symbolic link (for example into `~/dotfiles`),
            // protect its resolved target and its `.legion.lock` / `.legion-tmp-*` siblings from
            // non-admin file tools so a role with write access on `~/**` cannot edit the target
            // directly. (Path guards apply to file tools, not to roles that allow `exec`.)
            let default_auth_keys = home.join(".ssh").join("authorized_keys");
            if std::fs::symlink_metadata(&default_auth_keys)
                .is_ok_and(|meta| meta.file_type().is_symlink())
            {
                push_auth_keys_secret(
                    &mut places,
                    &mut prefixes,
                    "~/.ssh/authorized_keys",
                    default_auth_keys,
                );
            }
        }
        for extra in self.extra_secrets.iter() {
            let full = expand_tilde(Path::new(extra.as_str()), self.paths.home())
                .unwrap_or_else(|_| PathBuf::from(extra.as_str()));
            let resolved = resolve(&full)
                .or_else(|_| resolve_keeping_last(&full))
                .unwrap_or_else(|_| raw_fallback_resolved(&full));
            places.push(build_secret_place(
                extra.as_str().to_owned(),
                full,
                resolved,
            ));
        }
        // Protect Windows OpenSSH's administrator authorized_keys and host keys under
        // `%ProgramData%\ssh` alongside `~/.ssh` from non-admin file tools. (Path guards apply to
        // file tools, not to roles that allow `exec`.)
        #[cfg(windows)]
        {
            let program_data = legion_os::user::windows_program_data_dir()
                .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
            let full = program_data.join("ssh");
            let resolved = resolve(&full)
                .or_else(|_| resolve_keeping_last(&full))
                .unwrap_or_else(|_| raw_fallback_resolved(&full));
            places.push(build_secret_place(
                r"%ProgramData%\ssh".to_owned(),
                full.clone(),
                resolved,
            ));
            let admin_auth = full.join("administrators_authorized_keys");
            if std::fs::symlink_metadata(&admin_auth)
                .is_ok_and(|meta| meta.file_type().is_symlink())
            {
                push_auth_keys_secret(
                    &mut places,
                    &mut prefixes,
                    r"%ProgramData%\ssh\administrators_authorized_keys",
                    admin_auth,
                );
            }
        }
        // If `node.toml` configures `[devices] authorized_keys`, protect that file (following any
        // symlink) together with its `.legion.lock` and `.legion-tmp-*` atomic-write siblings from
        // non-admin file tools. (Path guards apply to file tools, not to roles that allow `exec`.)
        if let Ok(bytes) = std::fs::read(self.paths.config_file(crate::config::NodeFile::FILE))
            && let Ok(node) = crate::config::parse::<crate::config::NodeFile>(
                &self.paths.config_file(crate::config::NodeFile::FILE),
                &bytes,
            )
            && let Some(configured) = node.devices.authorized_keys
        {
            let full = expand_tilde(Path::new(&configured), self.paths.home())
                .unwrap_or_else(|_| PathBuf::from(&configured));
            push_auth_keys_secret(
                &mut places,
                &mut prefixes,
                "[devices] authorized_keys",
                full,
            );
        }
        (places, prefixes)
    }

    /// Refuses access to well-known secret locations for roles without `effect:admin`, unless the
    /// role's `paths` explicitly names the secret location or a subpath covering `place`.
    fn keep_out_of_secret_locations(
        &self,
        resolved: &Resolved,
        given: &Path,
        access: Access,
        resolved_roots: &[Resolved],
    ) -> Result<(), ErrorBody> {
        if self.allows_effect(Effect::Admin) {
            return Ok(());
        }
        let cache = self.protected_cache();
        let secrets = &cache.secrets;
        let prefixes = &cache.secret_prefixes;
        if secrets.is_empty() && prefixes.is_empty() {
            return Ok(());
        }
        let mut places = vec![Cow::Borrowed(resolved)];
        if resolved.link.is_some()
            && let Ok(target) = link_target(resolved)
        {
            places.push(Cow::Owned(target));
        }
        for place in &places {
            let ancestors: Vec<Handle> = place
                .existing
                .ancestors()
                .filter_map(|ancestor| Handle::from_path(ancestor).ok())
                .collect();
            for secret in secrets {
                let inside_secret = within_protected(place, &secret.resolved, Platform::HOST)
                    || secret
                        .identity
                        .as_deref()
                        .is_some_and(|identity| ancestors.contains(identity));
                if !inside_secret {
                    continue;
                }
                if secret_explicitly_opted_in_for_place(secret, place, &ancestors, resolved_roots) {
                    continue;
                }
                return Err(ErrorBody::new(
                    ErrorCode::Denied,
                    format!(
                        "{} may not {} {}: {} is on Legion's list of secret locations.",
                        self.principal,
                        access.verb(),
                        given.display(),
                        secret.label
                    ),
                )
                .with_hint(format!(
                    "Only roles with effect:admin may touch secret locations by default. To allow this role, the owner can name {} (or a path inside it) explicitly in the role's `paths` in policy.toml.",
                    secret.label
                ))
                .with_details(json!({
                    "path": resolved.path(),
                    "secret": secret.label,
                })));
            }
            for prefix in prefixes {
                if !prefix.matches_resolved(place) {
                    continue;
                }
                return Err(ErrorBody::new(
                    ErrorCode::Denied,
                    format!(
                        "{} may not {} {}: it is a staging file for {}.",
                        self.principal,
                        access.verb(),
                        given.display(),
                        prefix.label
                    ),
                )
                .with_hint(format!(
                    "Only roles with effect:admin may touch atomic-write staging files beside {}.",
                    prefix.label
                ))
                .with_details(json!({
                    "path": resolved.path(),
                    "secret": prefix.label,
                })));
            }
        }
        Ok(())
    }

    /// The node's configuration and state directories, resolved individually as far as they exist,
    /// with the identity of each that does.
    fn compute_own_dirs(&self) -> Vec<Result<OwnDir, (Place, PathBuf, Unresolved)>> {
        [
            (Place::Config, self.paths.config_dir()),
            (Place::State, self.paths.state_dir()),
        ]
        .into_iter()
        .map(|(place, dir)| {
            let resolved = resolve(dir).map_err(|error| (place, dir.to_path_buf(), error))?;
            let identity = if resolved.missing.is_empty() {
                Handle::from_path(&resolved.existing).ok().map(Arc::new)
            } else {
                None
            };
            #[cfg(unix)]
            let dev_ino = if resolved.missing.is_empty() {
                std::fs::symlink_metadata(&resolved.existing)
                    .ok()
                    .map(|m| (m.dev(), m.ino()))
            } else {
                None
            };
            Ok(OwnDir {
                place,
                dir: dir.to_path_buf(),
                resolved,
                identity,
                #[cfg(unix)]
                dev_ino,
            })
        })
        .collect()
    }

    /// Refuses a path in the node's own configuration or state to a role that does not allow
    /// `effect:admin`, and so does a last part that is a symbolic link leading there, which a tool
    /// writing through it would change.
    fn keep_out_of_own_dirs(
        &self,
        resolved: &Resolved,
        given: &Path,
        access: Access,
    ) -> Result<(), ErrorBody> {
        if self.allows_effect(Effect::Admin) {
            return Ok(());
        }
        let refuse = |reason: String| {
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
        let own: Vec<&OwnDir> = self
            .protected_cache()
            .own_dirs
            .iter()
            .map(|entry| {
                entry.as_ref().map_err(|(_, dir, error)| {
                    let reason = match error {
                        Unresolved::Invalid(error) => error.message.clone(),
                        Unresolved::Unclear(_, reason) => reason.clone(),
                    };
                    refuse(format!(
                        "Legion's own directory {} cannot be resolved ({reason}), so no path can be told apart from it",
                        dir.display()
                    ))
                    .with_hint("Run meta.doctor as the owner to see what is wrong with it.")
                })
            })
            .collect::<Result<_, _>>()?;
        let mut places = vec![Cow::Borrowed(resolved)];
        if resolved.link.is_some() {
            let target = link_target(resolved).map_err(|_| {
                refuse("it is a symbolic link, and where it leads cannot be told".to_owned())
            })?;
            places.push(Cow::Owned(target));
        }
        for place in &places {
            let ancestors: Vec<Handle> = place
                .existing
                .ancestors()
                .filter_map(|ancestor| Handle::from_path(ancestor).ok())
                .collect();
            for dir in &own {
                let inside = within_protected(place, &dir.resolved, Platform::HOST)
                    || dir
                        .identity
                        .as_deref()
                        .is_some_and(|identity| ancestors.contains(identity));
                if inside {
                    return Err(refuse(format!(
                        "it is in {}, {}, which only a role that allows effect:admin may read or change",
                        dir.dir.display(),
                        dir.place.describe()
                    ))
                    .with_hint("Legion's own files are read through its tools, such as audit.tail and meta.whoami; ask the owner of this machine for more.")
                    .with_details(json!({"path": resolved.path(), "directory": dir.dir})));
                }
            }
        }
        Ok(())
    }
}

/// One of the node's own directories.
#[derive(Debug)]
struct OwnDir {
    place: Place,
    /// As the node's paths name it.
    dir: PathBuf,
    resolved: Resolved,
    /// Which directory it is, when it exists, so a second name for it is known for what it is.
    identity: Option<Arc<Handle>>,
    #[cfg(unix)]
    dev_ino: Option<(u64, u64)>,
}

/// One built-in or policy-configured secret location resolved as far as it exists.
#[derive(Debug)]
struct SecretPlace {
    label: String,
    raw_path: PathBuf,
    resolved: Resolved,
    identity: Option<Arc<Handle>>,
    #[cfg(unix)]
    dev_ino: Option<(u64, u64)>,
}

/// Protected sibling filename prefix inside one directory (used for atomic-write staging files
/// `.<name>.legion-tmp-*` beside `authorized_keys`).
#[derive(Clone, Debug)]
struct SecretPrefix {
    label: String,
    reason: String,
    raw_dir: PathBuf,
    dir: Resolved,
    dir_identity: Option<Arc<Handle>>,
    file_prefix: String,
}

impl SecretPrefix {
    fn matches_name(&self, name: &std::ffi::OsStr) -> bool {
        let Some(name_str) = name.to_str() else {
            return false;
        };
        if Platform::HOST.ignores_case() {
            name_str
                .to_ascii_lowercase()
                .starts_with(&self.file_prefix.to_ascii_lowercase())
        } else {
            name_str.starts_with(&self.file_prefix)
        }
    }

    fn matches_resolved(&self, place: &Resolved) -> bool {
        let full = place.path();
        let Some(name) = full.file_name() else {
            return false;
        };
        if !self.matches_name(name) {
            return false;
        }
        let parent_resolved = if place.link.is_some() {
            Resolved::existing(place.existing.clone())
        } else if !place.missing.is_empty() {
            Resolved {
                existing: place.existing.clone(),
                missing: place.missing[..place.missing.len() - 1].to_vec(),
                link: None,
            }
        } else {
            let Some(parent) = place.existing.parent() else {
                return false;
            };
            Resolved::existing(parent.to_path_buf())
        };
        let same_path = within_protected(&parent_resolved, &self.dir, Platform::HOST)
            && within_protected(&self.dir, &parent_resolved, Platform::HOST);
        if same_path {
            return true;
        }
        self.dir_identity.as_deref().is_some_and(|id| {
            parent_resolved.missing.is_empty()
                && Handle::from_path(&parent_resolved.existing).ok().as_ref() == Some(id)
        })
    }

    fn matches_raw_path(&self, path: &Path) -> bool {
        let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
            return false;
        };
        if !self.matches_name(name) {
            return false;
        }
        let same_resolved = raw_path_within_resolved(parent, &self.dir, Platform::HOST)
            && within_protected(&self.dir, &raw_fallback_resolved(parent), Platform::HOST);
        let same_raw = raw_path_within_raw(parent, &self.raw_dir, Platform::HOST)
            && raw_path_within_raw(&self.raw_dir, parent, Platform::HOST);
        same_resolved
            || same_raw
            || self
                .dir_identity
                .as_deref()
                .is_some_and(|id| Handle::from_path(parent).ok().as_ref() == Some(id))
    }
}

/// Registers an `authorized_keys` path (following any symlink), its `.legion.lock` sibling, and
/// its `.legion-tmp-*` atomic-write staging prefix as protected secret locations.
fn push_auth_keys_secret(
    places: &mut Vec<SecretPlace>,
    prefixes: &mut Vec<SecretPrefix>,
    label: &str,
    full: PathBuf,
) {
    let resolved = resolve(&full)
        .or_else(|_| resolve_keeping_last(&full))
        .unwrap_or_else(|_| raw_fallback_resolved(&full));
    let target_path = resolved.path();
    places.push(build_secret_place(label.to_owned(), full.clone(), resolved));
    // If `full` itself was a symbolic link, protect the symlink path too so a non-admin role
    // cannot replace or unlink it either.
    if std::fs::symlink_metadata(&full).is_ok_and(|meta| meta.file_type().is_symlink())
        && let Ok(link_resolved) = resolve_keeping_last(&full)
    {
        places.push(build_secret_place(label.to_owned(), full, link_resolved));
    }
    let Some(parent) = target_path.parent().filter(|p| !p.as_os_str().is_empty()) else {
        return;
    };
    let base = target_path
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| !n.is_empty())
        .unwrap_or("authorized_keys");

    let lock_path = parent.join(format!(".{base}.legion.lock"));
    let lock_resolved = resolve(&lock_path)
        .or_else(|_| resolve_keeping_last(&lock_path))
        .unwrap_or_else(|_| raw_fallback_resolved(&lock_path));
    places.push(build_secret_place(
        format!("{label} lock file"),
        lock_path,
        lock_resolved,
    ));

    let dir_resolved = resolve(parent)
        .or_else(|_| resolve_keeping_last(parent))
        .unwrap_or_else(|_| raw_fallback_resolved(parent));
    let dir_identity = if dir_resolved.missing.is_empty() && dir_resolved.link.is_none() {
        Handle::from_path(&dir_resolved.existing).ok().map(Arc::new)
    } else {
        None
    };
    prefixes.push(SecretPrefix {
        label: label.to_owned(),
        reason: format!(
            "staging file beside {label} is protected; only roles with effect:admin may touch it"
        ),
        raw_dir: parent.to_path_buf(),
        dir: dir_resolved,
        dir_identity,
        file_prefix: format!(".{base}.legion-tmp-"),
    });
}

fn build_secret_place(label: String, raw_path: PathBuf, resolved: Resolved) -> SecretPlace {
    let identity = if resolved.missing.is_empty() && resolved.link.is_none() {
        Handle::from_path(&resolved.existing).ok().map(Arc::new)
    } else {
        None
    };
    #[cfg(unix)]
    let dev_ino = if resolved.missing.is_empty() && resolved.link.is_none() {
        std::fs::symlink_metadata(&resolved.existing)
            .ok()
            .map(|m| (m.dev(), m.ino()))
    } else {
        None
    };
    SecretPlace {
        label,
        raw_path,
        resolved,
        identity,
        #[cfg(unix)]
        dev_ino,
    }
}

/// Reason reported in `skipped` when a walk entry cannot be inspected to verify it is not protected.
const UNINSPECTABLE_PATH_REASON: &str =
    "path could not be inspected to verify it is not a protected location";

/// Reason reported in `skipped` when a symbolic link's target cannot be resolved during a walk.
const UNRESOLVABLE_LINK_REASON: &str =
    "symbolic link target could not be resolved to verify it is not a protected location";

/// Pre-resolved filter built by [`Grant::protected_filter`] so recursive directory walks skip
/// Legion's own configuration/state directories and secret locations using the exact same rules as
/// [`Grant::check_path`] and [`Grant::protected`].
#[derive(Debug)]
pub struct ProtectedFilter {
    entries: Vec<ProtectedEntry>,
    secret_prefixes: Vec<SecretPrefix>,
    explicit_roots: Vec<ExplicitRoot>,
    has_identities: bool,
    fail_closed_reason: Option<(Place, String)>,
}

#[derive(Debug)]
struct ExplicitRoot {
    resolved: Resolved,
    identity: Option<Arc<Handle>>,
    #[cfg(unix)]
    dev_ino: Option<(u64, u64)>,
}

#[derive(Debug)]
struct ProtectedEntry {
    place: Place,
    reason: String,
    raw_path: PathBuf,
    resolved: Resolved,
    identity: Option<Arc<Handle>>,
    #[cfg(unix)]
    dev_ino: Option<(u64, u64)>,
}

impl ProtectedFilter {
    /// A filter that blocks nothing (used for roles with `effect:admin` and unit tests).
    pub fn allow_all() -> ProtectedFilter {
        ProtectedFilter {
            entries: Vec::new(),
            secret_prefixes: Vec::new(),
            explicit_roots: Vec::new(),
            has_identities: false,
            fail_closed_reason: None,
        }
    }

    /// Whether this filter has no protected locations to skip and no fail-closed condition.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
            && self.secret_prefixes.is_empty()
            && self.fail_closed_reason.is_none()
    }

    /// Returns the matching protected [`Place`] and human-readable reason if `path` (or the target
    /// of `path` when it is a symbolic link) must be skipped during a directory walk.
    ///
    /// Because the walk root already passed [`Grant::check_path`] and every ancestor directory
    /// above `path` was already checked and pruned if blocked, normal entries are checked using
    /// only their own identity and path without re-resolving or walking ancestors. When a path or
    /// link target cannot be inspected or resolved, this fails closed after checking raw paths.
    pub fn blocked(&self, path: &Path) -> Option<(Place, &str)> {
        let mut ignored = 0usize;
        self.blocked_counting(path, &mut ignored)
    }

    /// Like [`ProtectedFilter::blocked`], counting every filesystem call performed so tests can
    /// verify the per-entry I/O bound deterministically.
    fn blocked_counting(&self, path: &Path, fs_calls: &mut usize) -> Option<(Place, &str)> {
        if self.is_empty() {
            return None;
        }
        *fs_calls += 1;
        let meta = match std::fs::symlink_metadata(path) {
            Ok(meta) => meta,
            Err(_) => {
                if let Some(hit) = self.match_path_only(path) {
                    return Some(hit);
                }
                if let Some((place, ref reason)) = self.fail_closed_reason {
                    return Some((place, reason.as_str()));
                }
                return Some((Place::Secret, UNINSPECTABLE_PATH_REASON));
            }
        };

        if meta.file_type().is_symlink() {
            // Check the symlink path itself first (0 extra FS calls), then follow where it leads.
            if let Some(hit) = self.match_path_only(path) {
                return Some(hit);
            }
            *fs_calls += 1;
            let raw_target = match std::fs::read_link(path) {
                Ok(target) => target,
                Err(_) => return Some((Place::Secret, UNRESOLVABLE_LINK_REASON)),
            };
            let joined = match path.parent() {
                Some(parent) => parent.join(raw_target),
                None => raw_target,
            };
            *fs_calls += 1;
            let target = match resolve(&joined) {
                Ok(target) => target,
                Err(_) => {
                    if let Some(hit) = self.match_path_only(&joined) {
                        return Some(hit);
                    }
                    if let Some((place, ref reason)) = self.fail_closed_reason {
                        return Some((place, reason.as_str()));
                    }
                    return Some((Place::Secret, UNRESOLVABLE_LINK_REASON));
                }
            };
            let target_id = if target.missing.is_empty() && self.has_identities {
                *fs_calls += 1;
                Handle::from_path(&target.existing).ok()
            } else {
                None
            };
            let target_ancestors: Vec<Handle> = if self.has_identities {
                target
                    .existing
                    .ancestors()
                    .filter_map(|anc| {
                        *fs_calls += 1;
                        Handle::from_path(anc).ok()
                    })
                    .collect()
            } else {
                Vec::new()
            };
            for entry in &self.entries {
                let matches_entry = within_protected(&target, &entry.resolved, Platform::HOST)
                    || (target_id.is_some() && target_id.as_ref() == entry.identity.as_deref())
                    || entry
                        .identity
                        .as_deref()
                        .is_some_and(|id| target_ancestors.contains(id));
                if !matches_entry {
                    continue;
                }
                if entry.place == Place::Secret
                    && self.target_opted_in(entry, &target, target_id.as_ref(), &target_ancestors)
                {
                    continue;
                }
                return Some((entry.place, entry.reason.as_str()));
            }
            for prefix in &self.secret_prefixes {
                if prefix.matches_resolved(&target) {
                    return Some((Place::Secret, prefix.reason.as_str()));
                }
            }
            if let Some((place, ref reason)) = self.fail_closed_reason {
                return Some((place, reason.as_str()));
            }
            return None;
        }

        // Normal file or directory: compare ONLY this entry's own identity and path.
        #[cfg(unix)]
        let dev_ino = Some((meta.dev(), meta.ino()));
        #[cfg(not(unix))]
        let entry_handle = if self.has_identities {
            *fs_calls += 1;
            Handle::from_path(path).ok()
        } else {
            None
        };

        for entry in &self.entries {
            #[cfg(unix)]
            let id_match = entry.dev_ino.is_some() && entry.dev_ino == dev_ino;
            #[cfg(not(unix))]
            let id_match =
                entry.identity.is_some() && entry.identity.as_deref() == entry_handle.as_ref();

            let matches_entry = id_match
                || raw_path_within_resolved(path, &entry.resolved, Platform::HOST)
                || raw_path_within_raw(path, &entry.raw_path, Platform::HOST);
            if !matches_entry {
                continue;
            }
            if entry.place == Place::Secret {
                let opted_in = self.explicit_roots.iter().any(|root| {
                    if !root_is_within_secret(
                        &root.resolved,
                        &entry.resolved,
                        entry.identity.as_deref(),
                    ) {
                        return false;
                    }
                    #[cfg(unix)]
                    let root_id_match = root.dev_ino.is_some() && root.dev_ino == dev_ino;
                    #[cfg(not(unix))]
                    let root_id_match = root.identity.is_some()
                        && root.identity.as_deref() == entry_handle.as_ref();
                    root_id_match || raw_path_within_resolved(path, &root.resolved, Platform::HOST)
                });
                if opted_in {
                    continue;
                }
            }
            return Some((entry.place, entry.reason.as_str()));
        }
        for prefix in &self.secret_prefixes {
            if prefix.matches_raw_path(path) {
                return Some((Place::Secret, prefix.reason.as_str()));
            }
        }
        if let Some((place, ref reason)) = self.fail_closed_reason {
            return Some((place, reason.as_str()));
        }
        None
    }

    fn match_path_only(&self, path: &Path) -> Option<(Place, &str)> {
        for entry in &self.entries {
            let matches_entry = raw_path_within_resolved(path, &entry.resolved, Platform::HOST)
                || raw_path_within_raw(path, &entry.raw_path, Platform::HOST);
            if !matches_entry {
                continue;
            }
            if entry.place == Place::Secret
                && self.explicit_roots.iter().any(|root| {
                    root_is_within_secret(
                        &root.resolved,
                        &entry.resolved,
                        entry.identity.as_deref(),
                    ) && raw_path_within_resolved(path, &root.resolved, Platform::HOST)
                })
            {
                continue;
            }
            return Some((entry.place, entry.reason.as_str()));
        }
        for prefix in &self.secret_prefixes {
            if prefix.matches_raw_path(path) {
                return Some((Place::Secret, prefix.reason.as_str()));
            }
        }
        None
    }

    fn target_opted_in(
        &self,
        entry: &ProtectedEntry,
        target: &Resolved,
        target_id: Option<&Handle>,
        target_ancestors: &[Handle],
    ) -> bool {
        self.explicit_roots.iter().any(|root| {
            if !root_is_within_secret(&root.resolved, &entry.resolved, entry.identity.as_deref()) {
                return false;
            }
            within_protected(target, &root.resolved, Platform::HOST)
                || root
                    .identity
                    .as_deref()
                    .is_some_and(|rid| target_id == Some(rid) || target_ancestors.contains(rid))
        })
    }

    /// Convenience wrapper returning whether `path` is blocked.
    pub fn is_blocked(&self, path: &Path) -> bool {
        self.blocked(path).is_some()
    }
}

/// Whether `root` names `secret` itself or a path strictly inside `secret` (a broad ancestor such
/// as `~/**` returns `false`).
fn root_is_within_secret(
    root: &Resolved,
    secret_resolved: &Resolved,
    secret_identity: Option<&Handle>,
) -> bool {
    if within(root, secret_resolved, Platform::HOST) {
        return true;
    }
    secret_identity.is_some_and(|id| {
        root.existing
            .ancestors()
            .any(|ancestor| Handle::from_path(ancestor).is_ok_and(|h| h == *id))
    })
}

/// Whether any configured root explicitly names the entire `secret` location itself.
fn secret_explicitly_opted_in_whole(secret: &SecretPlace, resolved_roots: &[Resolved]) -> bool {
    resolved_roots.iter().any(|root| {
        let same_path = within_protected(root, &secret.resolved, Platform::HOST)
            && within_protected(&secret.resolved, root, Platform::HOST);
        let same_handle = secret.identity.as_deref().is_some_and(|sid| {
            root.missing.is_empty() && Handle::from_path(&root.existing).ok().as_ref() == Some(sid)
        });
        same_path || same_handle
    })
}

/// Whether any configured root both explicitly names `secret` (or a subpath inside it) AND covers
/// `place`.
fn secret_explicitly_opted_in_for_place(
    secret: &SecretPlace,
    place: &Resolved,
    place_ancestors: &[Handle],
    resolved_roots: &[Resolved],
) -> bool {
    resolved_roots.iter().any(|root| {
        if !root_is_within_secret(root, &secret.resolved, secret.identity.as_deref()) {
            return false;
        }
        within_protected(place, root, Platform::HOST)
            || (root.missing.is_empty()
                && Handle::from_path(&root.existing)
                    .ok()
                    .is_some_and(|rid| place_ancestors.contains(&rid)))
    })
}

/// Where a last part that is a symbolic link leads, resolved as far as that exists.
fn link_target(resolved: &Resolved) -> Result<Resolved, Unresolved> {
    let link = resolved.path();
    let target = std::fs::read_link(&link).map_err(|error| {
        Unresolved::Unclear(
            ErrorCode::Failed,
            format!("{} cannot be read as a link ({error})", link.display()),
        )
    })?;
    // A relative target is relative to the directory the link is in, which is what exists.
    resolve(&resolved.existing.join(target))
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
#[derive(Clone, Debug)]
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

/// Lexical fallback [`Resolved`] when a path cannot be resolved on disk (for example a dangling
/// symlink or an unreadable parent), preserving its volume/root prefix and normal components so
/// raw-path comparisons still work.
fn raw_fallback_resolved(path: &Path) -> Resolved {
    let mut existing = PathBuf::new();
    let mut missing = Vec::new();
    for comp in path.components() {
        match comp {
            Component::Prefix(prefix) => existing.push(prefix.as_os_str()),
            Component::RootDir => existing.push(comp.as_os_str()),
            Component::Normal(name) => missing.push(name.to_owned()),
            Component::CurDir | Component::ParentDir => {}
        }
    }
    Resolved {
        existing,
        missing,
        link: None,
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

/// Pure platform-specific path component check run by [`Grant::check_path`].
///
/// On Windows, Win32 silently strips trailing dots and spaces from path components (so `.kube.`
/// opens `.kube` and `.ssh ` opens `.ssh`), treats a colon after the drive/share prefix as an NTFS
/// alternate data stream, and redirects reserved DOS device names (`CON`, `NUL`, `COM1`..`COM9`,
/// `LPT1`..`LPT9`, etc.) regardless of extension. Parameterised by [`Platform`] so every rule is
/// tested on every host OS.
fn validate_platform_path(
    path: &[u8],
    display: &Path,
    platform: Platform,
) -> Result<(), ErrorBody> {
    if platform != Platform::Windows {
        return Ok(());
    }
    if names_a_stream(path, platform) {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!(
                "{} names an alternate data stream, which Legion does not open.",
                display.display()
            ),
        )
        .with_hint(
            "Name the file itself: on Windows a colon after the drive selects a stream inside a file.",
        ));
    }
    let (_, rest) = volume(path, platform);
    for comp in rest
        .split(|byte| platform.separates(*byte))
        .filter(|part| !part.is_empty())
    {
        if comp == b"." || comp == b".." {
            continue;
        }
        if comp.ends_with(b".") || comp.ends_with(b" ") {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!(
                    "{} has path component {:?} ending in a dot or space, which Windows silently strips.",
                    display.display(),
                    String::from_utf8_lossy(comp)
                ),
            )
            .with_hint("Remove trailing dots and spaces from every path component on Windows."));
        }
        if is_windows_reserved_device_name(comp) {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!(
                    "{} uses the reserved Windows device name {:?}.",
                    display.display(),
                    String::from_utf8_lossy(comp)
                ),
            )
            .with_hint(
                "Avoid reserved Windows device names (CON, PRN, AUX, NUL, CONIN$, CONOUT$, COM1-COM9, LPT1-LPT9) in path components.",
            ));
        }
    }
    Ok(())
}

/// Whether `comp` matches a Win32 reserved device name (case-insensitively, ignoring any extension
/// or trailing dots/spaces).
fn is_windows_reserved_device_name(comp: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(comp) else {
        return false;
    };
    let stem = text
        .trim_end_matches(['.', ' '])
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end_matches(' ');
    if stem.is_empty() {
        return false;
    }
    if stem.eq_ignore_ascii_case("CON")
        || stem.eq_ignore_ascii_case("PRN")
        || stem.eq_ignore_ascii_case("AUX")
        || stem.eq_ignore_ascii_case("NUL")
        || stem.eq_ignore_ascii_case("CONIN$")
        || stem.eq_ignore_ascii_case("CONOUT$")
    {
        return true;
    }
    // `get` rather than slicing: the third byte of a name such as `naïve` falls inside a
    // character, and a slice there panics.
    let suffix = stem
        .get(..3)
        .filter(|prefix| prefix.eq_ignore_ascii_case("COM") || prefix.eq_ignore_ascii_case("LPT"))
        .and_then(|_| stem.get(3..));
    if let Some(suffix) = suffix {
        let mut chars = suffix.chars();
        if let (Some(digit), None) = (chars.next(), chars.next()) {
            return matches!(digit, '1'..='9' | '\u{00B9}' | '\u{00B2}' | '\u{00B3}');
        }
    }
    false
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

/// Like [`within`], ignoring whether either side existed when resolved so a cached protected
/// location still matches on case-insensitive platforms if it is created after the grant.
fn within_protected(path: &Resolved, root: &Resolved, platform: Platform) -> bool {
    let (path_volume, path_names) = names_of(path, platform);
    let (root_volume, root_names) = names_of(root, platform);
    path_volume == root_volume
        && path_names.len() >= root_names.len()
        && root_names
            .iter()
            .zip(&path_names)
            .all(|(root, name)| same_name_ignoring_existence(root.bytes, name.bytes, platform))
}

/// Pure in-memory prefix check comparing a raw `path` against a resolved protected `root`.
fn raw_path_within_resolved(path: &Path, root: &Resolved, platform: Platform) -> bool {
    let (path_volume, path_rest) = volume(path.as_os_str().as_encoded_bytes(), platform);
    let (root_volume, root_names) = names_of(root, platform);
    if path_volume != root_volume {
        return false;
    }
    let path_parts: Vec<&[u8]> = path_rest
        .split(|byte| platform.separates(*byte))
        .filter(|part| !part.is_empty())
        .collect();
    path_parts.len() >= root_names.len()
        && root_names
            .iter()
            .zip(&path_parts)
            .all(|(root_name, part)| same_name_ignoring_existence(root_name.bytes, part, platform))
}

/// Pure in-memory prefix check comparing two raw paths.
fn raw_path_within_raw(path: &Path, root: &Path, platform: Platform) -> bool {
    let (path_volume, path_rest) = volume(path.as_os_str().as_encoded_bytes(), platform);
    let (root_volume, root_rest) = volume(root.as_os_str().as_encoded_bytes(), platform);
    if path_volume != root_volume {
        return false;
    }
    let path_parts: Vec<&[u8]> = path_rest
        .split(|byte| platform.separates(*byte))
        .filter(|part| !part.is_empty())
        .collect();
    let root_parts: Vec<&[u8]> = root_rest
        .split(|byte| platform.separates(*byte))
        .filter(|part| !part.is_empty())
        .collect();
    path_parts.len() >= root_parts.len()
        && root_parts
            .iter()
            .zip(&path_parts)
            .all(|(r, p)| same_name_ignoring_existence(r, p, platform))
}

fn same_name_ignoring_existence(a: &[u8], b: &[u8], platform: Platform) -> bool {
    if a == b {
        return true;
    }
    if !platform.ignores_case() {
        return false;
    }
    match (std::str::from_utf8(a), std::str::from_utf8(b)) {
        (Ok(a), Ok(b)) => a.to_lowercase() == b.to_lowercase(),
        _ => false,
    }
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

    /// Paths for a check that never looks at the disk.
    fn nowhere() -> Paths {
        Paths::under("/legion")
    }

    /// A node's directories under `dir`, with its home at `dir/home`.
    fn under(dir: &Path) -> Paths {
        Paths::under(dir.join("legion")).with_home(dir.join("home"))
    }

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
                .authorize("owner", &spec("power.reboot", Effect::Power), &nowhere())
                .is_ok()
        );
        let error = empty
            .authorize(
                "agent:claude",
                &spec("meta.version", Effect::Read),
                &nowhere(),
            )
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Denied);
        assert!(error.message.contains("nobody"), "{}", error.message);
    }

    #[test]
    fn rules_match_names_globs_and_effects_and_deny_wins() {
        let policy = policy(DESIGN_EXAMPLE);
        let allowed = |who: &str, name: &str, effect: Effect| {
            policy
                .authorize(who, &spec(name, effect), &nowhere())
                .is_ok()
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
            .authorize(
                "agent:claude",
                &spec("fs.remove", Effect::Destroy),
                &nowhere(),
            )
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
            .authorize(
                "agent:gemini",
                &spec("meta.version", Effect::Read),
                &nowhere(),
            )
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
    fn only_the_owner_role_delegates() {
        let error = parse::<Policy>(
            Path::new("policy.toml"),
            b"[principals.laptop]\nrole = \"admin\"\n[roles.admin]\nallow = [\"*\"]\ndelegate = true\n",
        )
        .unwrap_err()
        .message;
        assert!(error.contains("[roles.admin]"), "{error}");
        assert!(error.contains("the owner's alone"), "{error}");
        let policy = policy(
            "[principals.laptop]\nrole = \"owner\"\n[principals.phone]\nrole = \"admin\"\n[roles.admin]\nallow = [\"*\"]\ndelegate = false\n",
        );
        assert_eq!(
            policy.resolve(Some("phone"), Some("laptop")).unwrap(),
            "phone"
        );
        assert_eq!(
            policy.resolve(Some("owner"), Some("phone")).unwrap(),
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
        let grant = policy.grant("agent:claude", &nowhere());
        assert_eq!(grant.role(), "operator");
        assert!(!grant.delegates());
        assert!(grant.allows(&spec("fs.read", Effect::Read)));
        assert!(!grant.allows(&spec("fs.remove", Effect::Destroy)));
        assert_eq!(grant.roots(), Some(vec!["~/projects".to_owned()]));
        assert!(Grant::owner(&nowhere()).delegates());
        assert_eq!(Grant::owner(&nowhere()).roots(), None);
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
            let grant = policy(DESIGN_EXAMPLE).grant("agent:claude", &under(dir.path()));
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
        let grant = policy(DESIGN_EXAMPLE).grant("agent:claude", &under(dir.path()));
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

    const KEEPERS: &str = r#"
[principals."agent:claude"]
role = "operator"

[principals.phone]
role = "observer"

[principals.keeper]
role = "keeper"

[roles.operator]
allow = ["effect:read", "effect:write"]
deny = ["fs.remove"]
paths = ["~/**"]

[roles.observer]
allow = ["effect:read"]

[roles.keeper]
allow = ["effect:read", "effect:write", "effect:admin"]
paths = ["~/**"]
"#;

    /// A node whose configuration and state live under its home, as they do by default.
    struct Own {
        _dir: tempfile::TempDir,
        paths: Paths,
        home: PathBuf,
    }

    impl Own {
        fn new() -> Own {
            let dir = tempfile::tempdir().unwrap();
            let home = dir.path().join("home");
            let paths = Paths::under(home.join(".legion")).with_home(&home);
            std::fs::create_dir_all(paths.config_dir()).unwrap();
            std::fs::write(paths.config_file("policy.toml"), KEEPERS).unwrap();
            std::fs::create_dir_all(paths.state_dir()).unwrap();
            std::fs::write(paths.audit_log(), "").unwrap();
            std::fs::create_dir_all(home.join("projects")).unwrap();
            let home = dunce::canonicalize(home).unwrap();
            Own {
                _dir: dir,
                paths,
                home,
            }
        }

        fn grant(&self, who: &str) -> Grant {
            policy(KEEPERS).grant(who, &self.paths)
        }

        fn check(&self, who: &str, path: &str, access: Access) -> Result<PathBuf, ErrorBody> {
            self.grant(who).check_path(path, access)
        }
    }

    #[test]
    fn legion_s_own_files_are_kept_from_every_role_without_the_admin_effect() {
        let own = Own::new();
        for (who, path, access) in [
            (
                "agent:claude",
                "~/.legion/config/policy.toml",
                Access::Write,
            ),
            ("agent:claude", "~/.legion/config", Access::Write),
            (
                "agent:claude",
                "~/.legion/config/tokens.toml",
                Access::Write,
            ),
            ("agent:claude", "~/.legion/config/new/deeper", Access::Write),
            ("agent:claude", "~/.legion/state/audit.jsonl", Access::Write),
            ("agent:claude", "~/.legion/state/audit.jsonl", Access::Read),
            (
                "agent:claude",
                "~/projects/../.legion/config/policy.toml",
                Access::Read,
            ),
            ("phone", "~/.legion/state/audit.jsonl", Access::Read),
        ] {
            let error = own.check(who, path, access).unwrap_err();
            assert_eq!(error.code, ErrorCode::Denied, "{who} {path}");
            assert!(
                error.message.contains("effect:admin") && error.message.contains(".legion"),
                "{}",
                error.message
            );
        }
        assert!(
            own.check("agent:claude", "~/projects/notes.txt", Access::Write)
                .is_ok()
        );
        assert!(own.check("agent:claude", "~/.legion", Access::Read).is_ok());
        assert!(
            own.check("keeper", "~/.legion/config/policy.toml", Access::Write)
                .is_ok()
        );
        let audit_log = own.home.join(".legion").join("state").join("audit.jsonl");
        assert!(
            Grant::owner(&own.paths)
                .check_path(&audit_log, Access::Write)
                .is_ok()
        );

        // Before the node has written anything there, its directories are kept all the same.
        std::fs::remove_dir_all(own.paths.config_dir()).unwrap();
        let error = own
            .check(
                "agent:claude",
                "~/.legion/config/policy.toml",
                Access::Write,
            )
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Denied);
    }

    #[cfg(unix)]
    #[test]
    fn links_do_not_lead_a_role_into_legion_s_own_files() {
        use std::os::unix::fs::symlink;
        let own = Own::new();
        let legion = own.home.join(".legion");
        let projects = own.home.join("projects");
        symlink(legion.join("config"), projects.join("conf")).unwrap();
        symlink(
            legion.join("config").join("policy.toml"),
            projects.join("policy"),
        )
        .unwrap();
        // It leads nowhere yet; written through, it would create tokens.toml.
        symlink(
            legion.join("config").join("tokens.toml"),
            projects.join("tokens"),
        )
        .unwrap();
        for path in [
            "~/projects/conf/policy.toml",
            "~/projects/policy",
            "~/projects/tokens",
        ] {
            for who in ["agent:claude", "phone"] {
                let checked = own.check(who, path, Access::Write);
                assert_eq!(
                    checked.map_err(|error| error.code),
                    Err(ErrorCode::Denied),
                    "{who} {path}"
                );
            }
        }
    }

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn another_spelling_does_not_lead_a_role_into_legion_s_own_files() {
        let own = Own::new();
        let error = own
            .check(
                "agent:claude",
                "~/.LEGION/Config/policy.toml",
                Access::Write,
            )
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Denied);
    }

    #[test]
    fn a_role_allows_an_effect_as_a_whole_only_by_the_effect_or_by_star() {
        let policy = policy(&format!(
            "{KEEPERS}\n[principals.most]\nrole = \"most\"\n[roles.most]\nallow = [\"*\"]\ndeny = [\"effect:admin\"]\n\n[principals.files]\nrole = \"files\"\n[roles.files]\nallow = [\"fs.*\"]\n"
        ));
        let allows =
            |who: &str, effect: Effect| policy.grant(who, &nowhere()).allows_effect(effect);
        assert!(allows("agent:claude", Effect::Write));
        assert!(!allows("agent:claude", Effect::Admin));
        assert!(!allows("agent:claude", Effect::Exec));
        assert!(allows("keeper", Effect::Admin));
        assert!(allows("owner", Effect::Admin));
        assert!(allows("most", Effect::Destroy));
        assert!(!allows("most", Effect::Admin));
        assert!(!allows("files", Effect::Write));
        assert!(!allows("stranger", Effect::Read));
    }

    #[test]
    fn unknown_effect_tools_require_star_without_effect_denies_and_accept_reported_effect() {
        let policy = policy(
            r#"
[principals.observer]
role = "observer"

[principals.ext_only]
role = "ext_only"

[principals.most]
role = "most"

[roles.observer]
allow = ["effect:read"]

[roles.ext_only]
allow = ["ext.*"]
deny = ["ext.blocked"]

[roles.most]
allow = ["*"]
deny = ["effect:admin"]
"#,
        );
        let paths = nowhere();

        // Unknown effect: only wildcard `*` without effect-level deny rules passes.
        assert!(
            policy
                .authorize_name("owner", "ext.probe", None, &paths)
                .is_ok()
        );
        assert_eq!(
            policy
                .authorize_name("most", "ext.probe", None, &paths)
                .unwrap_err()
                .code,
            ErrorCode::Denied
        );
        assert_eq!(
            policy
                .authorize_name("observer", "ext.probe", None, &paths)
                .unwrap_err()
                .code,
            ErrorCode::Denied
        );
        assert_eq!(
            policy
                .authorize_name("ext_only", "ext.probe", None, &paths)
                .unwrap_err()
                .code,
            ErrorCode::Denied
        );
        assert_eq!(
            policy
                .authorize_name("stranger", "ext.probe", None, &paths)
                .unwrap_err()
                .code,
            ErrorCode::Denied
        );

        // Pre-connect check: roles that cannot match fail without opening a session.
        assert!(policy.could_allow_with_remote_effect("observer", "ext.probe"));
        assert!(policy.could_allow_with_remote_effect("ext_only", "ext.probe"));
        assert!(!policy.could_allow_with_remote_effect("ext_only", "ext.blocked"));
        assert!(!policy.could_allow_with_remote_effect("ext_only", "other.tool"));
        assert!(!policy.could_allow_with_remote_effect("stranger", "ext.probe"));

        // Once the target reports the extension tool's effect, name and effect rules both apply.
        assert!(
            policy
                .authorize_name("observer", "ext.probe", Some(Effect::Read), &paths)
                .is_ok()
        );
        assert_eq!(
            policy
                .authorize_name("observer", "ext.zap", Some(Effect::Exec), &paths)
                .unwrap_err()
                .code,
            ErrorCode::Denied
        );
        assert!(
            policy
                .authorize_name("ext_only", "ext.probe", Some(Effect::Exec), &paths)
                .is_ok()
        );
        assert_eq!(
            policy
                .authorize_name("ext_only", "ext.blocked", Some(Effect::Read), &paths)
                .unwrap_err()
                .code,
            ErrorCode::Denied
        );
        assert!(
            policy
                .authorize_name("most", "ext.probe", Some(Effect::Read), &paths)
                .is_ok()
        );
        assert_eq!(
            policy
                .authorize_name("most", "ext.admin", Some(Effect::Admin), &paths)
                .unwrap_err()
                .code,
            ErrorCode::Denied
        );
    }

    #[test]
    fn the_places_a_whole_tree_must_not_be_taken_from_are_named() {
        let own = Own::new();
        let grant = own.grant("agent:claude");
        let home = &own.home;
        let legion = home.join(".legion");
        let root = home.ancestors().last().unwrap();
        let place = |path: &Path| grant.protected(path).unwrap();
        assert_eq!(place(root), Some(Place::Root));
        assert_eq!(place(home), Some(Place::Home));
        assert_eq!(place(home.parent().unwrap()), Some(Place::Home));
        assert_eq!(place(&legion), Some(Place::Config));
        assert_eq!(place(&legion.join("config")), Some(Place::Config));
        assert_eq!(place(&legion.join("state")), Some(Place::State));
        assert_eq!(place(&legion.join("config").join("policy.toml")), None);
        assert_eq!(place(&home.join("projects")), None);
        assert_eq!(place(&home.join("projects").join("new")), None);

        let refused = |who: &str, path: &Path| own.grant(who).check_tree(path).is_err();
        assert!(refused("agent:claude", &legion));
        assert!(refused("agent:claude", home));
        assert!(!refused("agent:claude", &home.join("projects")));
        assert!(!refused("keeper", &legion));
        assert!(refused("keeper", home));
        assert!(refused("owner", root));
        let error = own.grant("agent:claude").check_tree(&legion).unwrap_err();
        assert_eq!(error.code, ErrorCode::Denied);
        assert!(error.message.contains("configuration"), "{}", error.message);
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
        let grant = Grant::owner(&Paths::under(dir.path().join("legion")).with_home(dir.path()));
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

    #[test]
    fn secret_locations_are_refused_without_admin_unless_explicitly_named_in_paths() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(home.join(".ssh").join("allowed")).unwrap();
        std::fs::write(home.join(".ssh").join("id_ed25519"), "PRIVATE KEY").unwrap();
        std::fs::write(home.join(".ssh").join("allowed").join("note.txt"), "ok").unwrap();
        std::fs::create_dir_all(home.join(".aws")).unwrap();
        std::fs::write(home.join(".aws").join("credentials"), "secret").unwrap();
        std::fs::create_dir_all(home.join(".docker")).unwrap();
        std::fs::write(home.join(".docker").join("config.json"), "{}").unwrap();
        std::fs::write(home.join(".claude.json"), "{}").unwrap();
        std::fs::create_dir_all(home.join(".config").join("gh")).unwrap();
        std::fs::write(home.join(".config").join("gh").join("hosts.yml"), "tok").unwrap();
        std::fs::create_dir_all(home.join("projects")).unwrap();
        std::fs::write(home.join("projects").join("readme.md"), "hi").unwrap();

        let paths = Paths::under(dir.path().join("legion")).with_home(&home);
        let policy = crate::config::parse::<Policy>(
            Path::new("policy.toml"),
            br#"
[roles.wide_reader]
allow = ["effect:read", "effect:write"]

[roles.home_reader]
allow = ["effect:read"]
paths = ["~/**"]

[roles.ssh_optin]
allow = ["effect:read"]
paths = ["~/.ssh/**"]

[roles.partial_optin]
allow = ["effect:read"]
paths = ["~/**", "~/.ssh/allowed/**"]

[principals.wide]
role = "wide_reader"

[principals.homey]
role = "home_reader"

[principals.sshuser]
role = "ssh_optin"

[principals.partial]
role = "partial_optin"
"#,
        )
        .unwrap();

        // Unconfined non-admin is refused secret locations for both Read and Write, but can read normal files.
        let wide = policy.grant("wide", &paths);
        assert!(
            wide.check_path("~/projects/readme.md", Access::Read)
                .is_ok()
        );
        for secret in [
            "~/.ssh/id_ed25519",
            "~/.aws/credentials",
            "~/.docker/config.json",
            "~/.claude.json",
            "~/.config/gh/hosts.yml",
        ] {
            let err_r = wide.check_path(secret, Access::Read).unwrap_err();
            assert_eq!(err_r.code, ErrorCode::Denied, "{secret}");
            assert!(
                err_r.message.contains("secret locations"),
                "{secret}: {}",
                err_r.message
            );
            assert!(
                err_r.hint.as_deref().is_some_and(|h| h.contains("paths")),
                "{secret}: {:?}",
                err_r.hint
            );
            let err_w = wide.check_path(secret, Access::Write).unwrap_err();
            assert_eq!(err_w.code, ErrorCode::Denied, "write {secret}");
        }

        // Broad `~/**` confinement does NOT count as explicitly naming a secret location.
        let homey = policy.grant("homey", &paths);
        assert!(
            homey
                .check_path("~/projects/readme.md", Access::Read)
                .is_ok()
        );
        assert_eq!(
            homey
                .check_path("~/.ssh/id_ed25519", Access::Read)
                .unwrap_err()
                .code,
            ErrorCode::Denied
        );
        assert_eq!(
            homey
                .check_path("~/.claude.json", Access::Read)
                .unwrap_err()
                .code,
            ErrorCode::Denied
        );

        // Explicitly naming `~/.ssh/**` opts in for `.ssh`, while other secrets stay refused.
        let sshuser = policy.grant("sshuser", &paths);
        assert!(
            sshuser
                .check_path("~/.ssh/id_ed25519", Access::Read)
                .is_ok()
        );
        assert_eq!(
            sshuser
                .check_path("~/.aws/credentials", Access::Read)
                .unwrap_err()
                .code,
            ErrorCode::Denied
        );

        // Naming a subpath `~/.ssh/allowed/**` alongside `~/**` allows only that subpath, not sibling keys.
        let partial = policy.grant("partial", &paths);
        assert!(
            partial
                .check_path("~/.ssh/allowed/note.txt", Access::Read)
                .is_ok()
        );
        assert_eq!(
            partial
                .check_path("~/.ssh/id_ed25519", Access::Read)
                .unwrap_err()
                .code,
            ErrorCode::Denied
        );

        // Owner (admin) can access every secret location.
        let owner = policy.grant("owner", &paths);
        assert!(owner.check_path("~/.ssh/id_ed25519", Access::Read).is_ok());
        assert!(owner.check_path("~/.claude.json", Access::Write).is_ok());

        // `protected` and `check_tree` treat secret locations and ancestors holding them as Place::Secret.
        assert_eq!(
            wide.protected(&home.join(".ssh")).unwrap(),
            Some(Place::Secret)
        );
        assert_eq!(
            wide.protected(&home.join(".config")).unwrap(),
            Some(Place::Secret)
        );
        assert!(wide.check_tree(&home.join(".config")).is_err());
        assert_eq!(sshuser.protected(&home.join(".ssh")).unwrap(), None);
        assert_eq!(owner.protected(&home.join(".ssh")).unwrap(), None);

        // Symlinks into secret locations are also refused.
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(
                home.join(".ssh").join("id_ed25519"),
                home.join("projects").join("keylink"),
            )
            .unwrap();
            let err = wide
                .check_path("~/projects/keylink", Access::Read)
                .unwrap_err();
            assert_eq!(err.code, ErrorCode::Denied);
            assert!(err.message.contains("secret locations"), "{}", err.message);
        }

        // ProtectedFilter blocks secret locations for non-admin and skips nothing for owner.
        let filter = wide.protected_filter();
        assert_eq!(
            filter.blocked(&home.join(".ssh")).map(|(p, _)| p),
            Some(Place::Secret)
        );
        assert_eq!(
            filter.blocked(&home.join(".claude.json")).map(|(p, _)| p),
            Some(Place::Secret)
        );
        assert!(
            filter
                .blocked(&home.join("projects").join("readme.md"))
                .is_none()
        );
        assert!(owner.protected_filter().is_empty());
    }

    #[test]
    fn check_create_refuses_symlinks_entries_and_missing_parents_of_secret_locations() {
        let dir = tempfile::tempdir().unwrap();
        let paths = under(dir.path());
        let home = paths.home().unwrap();
        std::fs::create_dir_all(home.join("projects")).unwrap();
        std::fs::create_dir_all(home.join(".config")).unwrap();

        let policy = policy(
            r#"
default = "owner"

[roles.wide]
allow = ["effect:read", "effect:write", "effect:destroy"]

[principals.wide]
role = "wide"
"#,
        );
        let wide = policy.grant("wide", &paths);
        let owner = policy.grant("owner", &paths);

        // `~/.codex` is absent, and is the proper ancestor of secret `~/.codex/auth.json`.
        // Every entry kind (symlink, dir, file) at `~/.codex` is refused.
        for kind in [CreateKind::Symlink, CreateKind::Dir, CreateKind::File] {
            let err = wide
                .check_create(&home.join(".codex"), kind, false)
                .unwrap_err();
            assert_eq!(err.code, ErrorCode::Denied, "{kind:?}");
            assert!(err.message.contains(".codex/auth.json"), "{}", err.message);
        }

        // Creating a file or directory under missing `~/.codex` with create_parents=true is refused.
        let err = wide
            .check_create(
                &home.join(".codex").join("other.txt"),
                CreateKind::File,
                true,
            )
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Denied);
        let err = wide
            .check_create(&home.join(".codex").join("sub"), CreateKind::Dir, true)
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Denied);

        // A symlink at existing `~/.config` (ancestor of `~/.config/gh`) is refused.
        assert_eq!(
            wide.check_create(&home.join(".config"), CreateKind::Symlink, false)
                .unwrap_err()
                .code,
            ErrorCode::Denied
        );
        // Creating a sibling file inside ALREADY-EXISTING `~/.config` is allowed.
        assert!(
            wide.check_create(
                &home.join(".config").join("myapp").join("settings.toml"),
                CreateKind::File,
                true,
            )
            .is_ok()
        );
        // Normal files under `~/projects` are allowed.
        assert!(
            wide.check_create(
                &home.join("projects").join("sub").join("a.txt"),
                CreateKind::File,
                true,
            )
            .is_ok()
        );
        // Admin (owner) is allowed.
        assert!(
            owner
                .check_create(&home.join(".codex"), CreateKind::Symlink, false)
                .is_ok()
        );
    }

    #[test]
    fn windows_path_validation_refuses_trailing_dot_space_and_reserved_devices_on_every_platform() {
        let check = |raw: &str, platform: Platform| {
            validate_platform_path(raw.as_bytes(), Path::new(raw), platform)
        };

        for bad in [
            r"C:\Users\me\.kube.\config",
            r"C:\Users\me\.ssh \authorized_keys",
            r"C:\Users\me\projects\file.",
            r"C:\Users\me\projects\file ",
            r"C:\Users\me\projects\...",
            r"\\?\C:\Users\me\.kube.\config",
            r"\\server\share\.ssh \id_ed25519",
            r"C:\Users\me\projects\CON",
            r"C:\Users\me\projects\nul.txt",
            r"C:\Users\me\projects\COM1.log",
            "C:\\Users\\me\\projects\\com\u{00B9}",
            r"C:\Users\me\projects\LPT9",
            "C:\\Users\\me\\projects\\lpt\u{00B3}.dat",
            r"C:\Users\me\projects\CONIN$",
            r"C:\Users\me\projects\CONOUT$",
            r"C:\Users\me\projects\aux .txt",
            r"C:\Users\me\projects\prn",
            r"C:\Users\me\projects\file.txt:stream",
        ] {
            let err = check(bad, Platform::Windows).unwrap_err();
            assert_eq!(err.code, ErrorCode::InvalidArgs, "{bad}");
        }

        for good_windows in [
            r"C:\Users\me\.kube\config",
            r"C:\Users\me\.ssh\authorized_keys",
            r"C:\Users\me\projects\..\ok\file.txt",
            r"C:\Users\me\projects\.\file.txt",
            r"C:\Users\me\projects\console.txt",
            r"C:\Users\me\projects\COM0",
            r"\\?\C:\Users\me\projects\readme.md",
        ] {
            assert!(
                check(good_windows, Platform::Windows).is_ok(),
                "{good_windows}"
            );
        }

        // Unix and macOS do not strip trailing dots/spaces or reserve DOS device names.
        for non_windows in [Platform::Unix, Platform::Mac] {
            for path in [
                "/home/me/.kube./config",
                "/home/me/.ssh /authorized_keys",
                "/home/me/projects/CON",
                "/home/me/projects/file:stream",
            ] {
                assert!(check(path, non_windows).is_ok(), "{non_windows:?} {path}");
            }
        }
    }

    #[test]
    fn windows_device_name_check_takes_names_whose_third_byte_is_inside_a_character() {
        let check =
            |raw: &str| validate_platform_path(raw.as_bytes(), Path::new(raw), Platform::Windows);
        for name in [
            "naïve & “quoted” $name %x%.txt",
            "coñ",
            "coñ1.txt",
            "lpτ1",
            "über",
            "日本語.txt",
            "Ελληνικά",
            "😀",
        ] {
            let raw = format!(r"C:\Users\me\projects\{name}");
            assert!(check(&raw).is_ok(), "{raw}");
        }
        assert!(check("C:\\Users\\me\\projects\\COM\u{00B2}.txt").is_err());
    }

    #[test]
    fn protected_filter_caches_identities_and_bounds_fs_calls_per_entry() {
        let dir = tempfile::tempdir().unwrap();
        let paths = under(dir.path());
        let home = paths.home().unwrap();
        std::fs::create_dir_all(paths.config_dir()).unwrap();
        std::fs::create_dir_all(paths.state_dir()).unwrap();

        // Populate several real secret locations on disk.
        let secret_dirs = [
            ".ssh",
            ".gnupg",
            ".aws",
            ".kube",
            ".config/gh",
            ".config/gcloud",
        ];
        for rel in secret_dirs {
            std::fs::create_dir_all(home.join(rel)).unwrap();
        }
        std::fs::write(home.join(".claude.json"), "{}").unwrap();
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        std::fs::write(home.join(".codex/auth.json"), "{}").unwrap();

        // Create 200 regular files nested 5 directories deep under ~/projects.
        let deep_root = home.join("projects/a/b/c/d");
        std::fs::create_dir_all(&deep_root).unwrap();
        let mut normal_entries = Vec::new();
        for i in 0..200 {
            let file = deep_root.join(format!("file_{i:03}.txt"));
            std::fs::write(&file, b"ok").unwrap();
            normal_entries.push(file);
        }

        let policy = policy(
            r#"
default = "owner"
[roles.wide]
allow = ["effect:read", "effect:write"]
[principals.wide]
role = "wide"
"#,
        );
        let wide = policy.grant("wide", &paths);
        let filter = wide.protected_filter();

        // Walking 200 deep entries must do O(1) filesystem calls per entry (<= 1 on Unix, <= 2 on Windows).
        let max_per_entry = if cfg!(unix) { 1 } else { 2 };
        let mut total_calls = 0usize;
        for entry in &normal_entries {
            let mut calls = 0usize;
            assert!(
                filter.blocked_counting(entry, &mut calls).is_none(),
                "{}",
                entry.display()
            );
            assert!(
                calls <= max_per_entry,
                "expected <= {max_per_entry} fs calls for {}, got {calls}",
                entry.display()
            );
            total_calls += calls;
        }
        assert!(total_calls <= normal_entries.len() * max_per_entry);

        // Every secret location and Legion's own dirs are still blocked in O(1) calls.
        for secret in [
            home.join(".ssh"),
            home.join(".gnupg"),
            home.join(".aws"),
            home.join(".kube"),
            home.join(".claude.json"),
            home.join(".codex/auth.json"),
            home.join(".config/gh"),
            home.join(".config/gcloud"),
        ] {
            let mut calls = 0usize;
            let hit = filter.blocked_counting(&secret, &mut calls);
            assert_eq!(
                hit.map(|(p, _)| p),
                Some(Place::Secret),
                "{}",
                secret.display()
            );
            assert!(calls <= max_per_entry, "{}: {calls}", secret.display());
        }
        for own in [paths.config_dir(), paths.state_dir()] {
            let mut calls = 0usize;
            assert!(filter.blocked_counting(own, &mut calls).is_some());
            assert!(calls <= max_per_entry, "{}: {calls}", own.display());
        }

        // A secret location created AFTER the grant's cache was populated is still caught by check_path.
        std::fs::write(home.join(".netrc"), "machine example").unwrap();
        assert_eq!(
            wide.check_path("~/.netrc", Access::Read).unwrap_err().code,
            ErrorCode::Denied
        );
    }

    #[test]
    fn walk_helpers_fail_closed_with_raw_path_fallback_when_resolution_fails() {
        let dir = tempfile::tempdir().unwrap();
        let paths = under(dir.path());
        let home = paths.home().unwrap();
        std::fs::create_dir_all(home.join("projects")).unwrap();
        std::fs::create_dir_all(paths.config_dir()).unwrap();
        std::fs::create_dir_all(paths.state_dir()).unwrap();

        let policy = policy(
            r#"
default = "owner"
[roles.wide]
allow = ["effect:read", "effect:write"]
[principals.wide]
role = "wide"
"#,
        );

        // 1. `blocked` fails closed when a path cannot be inspected, falling back to raw-path match first.
        let wide = policy.grant("wide", &paths);
        let filter = wide.protected_filter();
        let missing_secret = home.join(".ssh").join("id_ed25519");
        let (place, reason) = filter
            .blocked(&missing_secret)
            .expect("missing secret path must be blocked by raw fallback");
        assert_eq!(place, Place::Secret);
        assert!(reason.contains(".ssh"), "{reason}");

        let missing_normal = home.join("projects").join("vanished.txt");
        let (_, reason) = filter
            .blocked(&missing_normal)
            .expect("uninspectable path must fail closed");
        assert!(reason.contains("could not be inspected"), "{reason}");

        // 2. Unresolvable symlink loop in walk fails closed; dangling secret location (`~/.ssh`) is NOT dropped.
        #[cfg(unix)]
        {
            let loop_a = home.join("projects").join("loop_a");
            let loop_b = home.join("projects").join("loop_b");
            std::os::unix::fs::symlink(&loop_b, &loop_a).unwrap();
            std::os::unix::fs::symlink(&loop_a, &loop_b).unwrap();
            let (_, reason) = filter
                .blocked(&loop_a)
                .expect("unresolvable symlink loop must fail closed in walk");
            assert!(reason.contains("could not be resolved"), "{reason}");

            // Make `~/.ssh` itself a dangling symlink so `resolve(&full)` fails: `compute_secret_locations`
            // must NOT silently drop `~/.ssh`!
            let ssh_path = home.join(".ssh");
            std::os::unix::fs::symlink(dir.path().join("missing_ssh_target"), &ssh_path).unwrap();
            let wide2 = policy.grant("wide", &paths);
            let filter2 = wide2.protected_filter();
            let (place, reason) = filter2
                .blocked(&ssh_path)
                .expect("dangling ~/.ssh must still be blocked");
            assert_eq!(place, Place::Secret);
            assert!(reason.contains(".ssh"), "{reason}");

            // 3. Unresolvable `config_dir` (dangling symlink) must NOT be dropped by `protected_filter`:
            // raw-path matches `config_dir` directly AND other paths fail closed with a clear reason.
            let broken_dir = tempfile::tempdir().unwrap();
            let broken_paths = under(broken_dir.path());
            let broken_home = broken_paths.home().unwrap();
            std::fs::create_dir_all(broken_home.join("projects")).unwrap();
            std::fs::write(broken_home.join("projects").join("ok.txt"), b"ok").unwrap();
            std::fs::create_dir_all(broken_paths.config_dir().parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(
                broken_dir.path().join("missing_config_target"),
                broken_paths.config_dir(),
            )
            .unwrap();
            let broken_grant = policy.grant("wide", &broken_paths);
            let broken_filter = broken_grant.protected_filter();
            assert!(!broken_filter.is_empty());
            let (cfg_place, cfg_reason) = broken_filter
                .blocked(broken_paths.config_dir())
                .expect("unresolvable config_dir must still be blocked by raw path");
            assert_eq!(cfg_place, Place::Config);
            assert!(cfg_reason.contains("configuration"), "{cfg_reason}");

            let (other_place, other_reason) = broken_filter
                .blocked(&broken_home.join("projects").join("ok.txt"))
                .expect("unresolved own_dir must fail closed for walk entries");
            assert_eq!(other_place, Place::Config);
            assert!(
                other_reason.contains("could not be resolved"),
                "{other_reason}"
            );
        }
    }

    #[test]
    fn expanded_secret_locations_and_top_level_policy_secrets_are_enforced_identically() {
        let dir = tempfile::tempdir().unwrap();
        let paths = under(dir.path());
        let home = paths.home().unwrap();
        std::fs::create_dir_all(home.join("projects")).unwrap();
        std::fs::create_dir_all(home.join("extra").join("vault")).unwrap();
        std::fs::write(home.join("extra").join("vault").join("key.pem"), "SECRET").unwrap();
        std::fs::write(home.join(".npmrc"), "//registry/:_authToken=x").unwrap();
        std::fs::write(home.join(".bash_history"), "ls\n").unwrap();

        // Every newly required entry is present in SECRET_LOCATIONS.
        for required in [
            ".config/rclone",
            ".config/op",
            ".vault-token",
            ".terraform.d/credentials.tfrc.json",
            ".cargo/credentials.toml",
            ".pypirc",
            ".config/microsoft-edge",
            ".thunderbird",
            "Library/Application Support/BraveSoftware/Brave-Browser",
            "Library/Application Support/Microsoft Edge",
            "Library/Cookies",
            "Library/Containers/com.apple.Safari/Data/Library/Cookies",
            "AppData/Roaming/Microsoft/Credentials",
            "AppData/Roaming/Microsoft/Protect",
            "AppData/Local/Microsoft/Credentials",
            "AppData/Local/BraveSoftware/Brave-Browser/User Data",
        ] {
            assert!(
                SECRET_LOCATIONS.contains(&required),
                "SECRET_LOCATIONS missing {required}"
            );
        }

        // Without custom `secrets`, `.npmrc` and `.bash_history` are deliberately NOT blocked,
        // while newly added built-ins such as `.config/rclone`, `.cargo/credentials.toml`, and
        // `.vault-token` ARE blocked.
        let default_pol = policy(
            r#"
default = "owner"
[roles.wide]
allow = ["effect:read", "effect:write"]
[principals.wide]
role = "wide"
"#,
        );
        let wide_default = default_pol.grant("wide", &paths);
        assert!(wide_default.check_path("~/.npmrc", Access::Read).is_ok());
        assert!(
            wide_default
                .check_path("~/.bash_history", Access::Read)
                .is_ok()
        );
        for builtin in [
            "~/.config/rclone/rclone.conf",
            "~/.config/op/config",
            "~/.vault-token",
            "~/.terraform.d/credentials.tfrc.json",
            "~/.cargo/credentials.toml",
            "~/.pypirc",
            "~/.config/microsoft-edge/Default/Cookies",
            "~/.thunderbird/profiles.ini",
        ] {
            assert_eq!(
                wide_default
                    .check_path(builtin, Access::Read)
                    .unwrap_err()
                    .code,
                ErrorCode::Denied,
                "{builtin}"
            );
        }

        // Top-level `secrets = [...]` in policy.toml is enforced identically to built-ins across
        // check_path, check_create, protected/check_tree, protected_filter, and role opt-in.
        let custom_pol = policy(
            r#"
default = "owner"
secrets = ["~/.npmrc", "~/extra/vault", "~/missing_parent/token.txt"]

[roles.wide]
allow = ["effect:read", "effect:write"]

[roles.home_only]
allow = ["effect:read"]
paths = ["~/**"]

[roles.vault_reader]
allow = ["effect:read"]
paths = ["~/extra/vault/**"]

[principals.wide]
role = "wide"

[principals.homey]
role = "home_only"

[principals.vault]
role = "vault_reader"
"#,
        );
        let wide = custom_pol.grant("wide", &paths);
        let homey = custom_pol.grant("homey", &paths);
        let vault = custom_pol.grant("vault", &paths);
        let owner = custom_pol.grant("owner", &paths);

        assert_eq!(
            wide.check_path("~/.npmrc", Access::Read).unwrap_err().code,
            ErrorCode::Denied
        );
        assert_eq!(
            homey
                .check_path("~/extra/vault/key.pem", Access::Read)
                .unwrap_err()
                .code,
            ErrorCode::Denied
        );
        assert_eq!(
            wide.check_create(&home.join("missing_parent"), CreateKind::Symlink, false)
                .unwrap_err()
                .code,
            ErrorCode::Denied
        );
        assert_eq!(
            wide.protected(&home.join("extra").join("vault")).unwrap(),
            Some(Place::Secret)
        );
        assert_eq!(
            wide.protected_filter()
                .blocked(&home.join("extra").join("vault"))
                .map(|(p, _)| p),
            Some(Place::Secret)
        );
        assert!(
            vault
                .check_path("~/extra/vault/key.pem", Access::Read)
                .is_ok()
        );
        assert!(owner.check_path("~/.npmrc", Access::Read).is_ok());
    }

    #[test]
    fn configured_devices_authorized_keys_is_closed_to_non_admin_roles() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let ssh_dir = dir.path().join("custom-ssh");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&ssh_dir).unwrap();
        let auth_keys = ssh_dir.join("authorized_keys");
        std::fs::write(&auth_keys, "").unwrap();

        let paths = Paths::under(dir.path().join("legion")).with_home(&home);
        std::fs::create_dir_all(paths.config_dir()).unwrap();
        let escaped = auth_keys.display().to_string().replace('\\', "\\\\");
        std::fs::write(
            paths.config_file("node.toml"),
            format!("[devices]\nauthorized_keys = \"{escaped}\"\n"),
        )
        .unwrap();

        let pol = policy(&format!(
            r#"
default = "owner"

[roles.wide]
allow = ["effect:read", "effect:write"]

[roles.parent_scoped]
allow = ["effect:read", "effect:write"]
paths = ["{}/**"]

[principals.wide]
role = "wide"

[principals.scoped]
role = "parent_scoped"
"#,
            ssh_dir.display().to_string().replace('\\', "\\\\")
        ));
        let wide = pol.grant("wide", &paths);
        let scoped = pol.grant("scoped", &paths);
        let owner = pol.grant("owner", &paths);

        for grant in [&wide, &scoped] {
            assert_eq!(
                grant.check_path(&auth_keys, Access::Read).unwrap_err().code,
                ErrorCode::Denied
            );
            assert_eq!(
                grant
                    .check_path(&auth_keys, Access::Write)
                    .unwrap_err()
                    .code,
                ErrorCode::Denied
            );
            assert_eq!(
                grant
                    .check_create(&auth_keys, CreateKind::File, false)
                    .unwrap_err()
                    .code,
                ErrorCode::Denied
            );
            assert_eq!(
                grant.protected_filter().blocked(&auth_keys).map(|(p, _)| p),
                Some(Place::Secret)
            );
        }
        assert!(owner.check_path(&auth_keys, Access::Write).is_ok());

        // Lock and atomic-write staging siblings beside configured authorized_keys are also closed.
        let lock_sibling = ssh_dir.join(".authorized_keys.legion.lock");
        let tmp_sibling = ssh_dir.join(".authorized_keys.legion-tmp-123-0-456");
        let unrelated = ssh_dir.join("notes.txt");
        for sibling in [&lock_sibling, &tmp_sibling] {
            for grant in [&wide, &scoped] {
                assert_eq!(
                    grant.check_path(sibling, Access::Write).unwrap_err().code,
                    ErrorCode::Denied,
                    "{}",
                    sibling.display()
                );
                assert_eq!(
                    grant
                        .check_create(sibling, CreateKind::File, false)
                        .unwrap_err()
                        .code,
                    ErrorCode::Denied,
                    "{}",
                    sibling.display()
                );
                assert_eq!(
                    grant.protected_filter().blocked(sibling).map(|(p, _)| p),
                    Some(Place::Secret),
                    "{}",
                    sibling.display()
                );
            }
            assert!(owner.check_path(sibling, Access::Write).is_ok());
        }
        assert!(scoped.check_path(&unrelated, Access::Write).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_authorized_keys_target_and_siblings_are_closed_to_non_admin_roles() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let dotfiles = home.join("dotfiles");
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::create_dir_all(&dotfiles).unwrap();

        let target_keys = dotfiles.join("authorized_keys");
        std::fs::write(&target_keys, "# existing\n").unwrap();
        symlink(&target_keys, home.join(".ssh").join("authorized_keys")).unwrap();

        let paths = Paths::under(dir.path().join("legion")).with_home(&home);
        let pol = policy(
            r#"
default = "owner"

[roles.home_writer]
allow = ["effect:read", "effect:write"]
paths = ["~/**"]

[principals.helper]
role = "home_writer"
"#,
        );
        let helper = pol.grant("helper", &paths);
        let owner = pol.grant("owner", &paths);

        let lock_sibling = dotfiles.join(".authorized_keys.legion.lock");
        let tmp_sibling = dotfiles.join(".authorized_keys.legion-tmp-99-1-12345");
        let other_dotfile = dotfiles.join("gitconfig");

        for protected_path in [&target_keys, &lock_sibling, &tmp_sibling] {
            assert_eq!(
                helper
                    .check_path(protected_path, Access::Read)
                    .unwrap_err()
                    .code,
                ErrorCode::Denied,
                "read {}",
                protected_path.display()
            );
            assert_eq!(
                helper
                    .check_path(protected_path, Access::Write)
                    .unwrap_err()
                    .code,
                ErrorCode::Denied,
                "write {}",
                protected_path.display()
            );
            assert_eq!(
                helper
                    .check_create(protected_path, CreateKind::File, false)
                    .unwrap_err()
                    .code,
                ErrorCode::Denied,
                "create {}",
                protected_path.display()
            );
            assert_eq!(
                helper
                    .protected_filter()
                    .blocked(protected_path)
                    .map(|(p, _)| p),
                Some(Place::Secret),
                "walk {}",
                protected_path.display()
            );
            assert!(owner.check_path(protected_path, Access::Write).is_ok());
        }
        // Unrelated files in the same dotfiles directory remain writable by home_writer.
        assert!(helper.check_path(&other_dotfile, Access::Write).is_ok());
    }
}
