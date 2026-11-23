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
    /// the last part is kept as given, so a tool that removes or renames a symbolic link acts on
    /// the link and not on what it points to.
    ///
    /// When the role confines file tools to `paths`, the result must lie under one of them, and so
    /// must the file a symbolic link in the last part leads to: a link inside a root cannot be a
    /// way out of it. A path whose parent directories do not exist yet is checked as it will be
    /// once they do, and `..` among those is refused, since nothing can say where it would lead.
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
        let resolved = resolve_keeping_last(&expanded)?;

        let Some(roots) = &self.rules.paths else {
            return Ok(resolved);
        };
        let mut resolved_roots = Vec::with_capacity(roots.len());
        for root in roots {
            let root = expand_tilde(Path::new(root.as_str()), self.home.as_deref())?;
            resolved_roots.push(resolve(&root)?);
        }
        let inside = |path: &Path| resolved_roots.iter().any(|root| path.starts_with(root));
        let outside = || {
            ErrorBody::new(
                ErrorCode::Denied,
                format!(
                    "{} may not {} {}: its role {} confines file tools to {}.",
                    self.principal,
                    access.verb(),
                    given.display(),
                    self.role,
                    roots.iter().map(PathRoot::as_str).collect::<Vec<_>>().join(", ")
                ),
            )
            .with_hint("Use a path under one of those directories, or ask the owner to add the directory to the role's paths in policy.toml.")
            .with_details(json!({"path": resolved, "roots": resolved_roots}))
        };
        if !inside(&resolved) {
            return Err(outside());
        }
        // The last part was kept as given; if it is a symbolic link, where it leads must be inside
        // as well. A link that leads nowhere cannot be checked, and writing through it would
        // create a file wherever it points.
        let is_link =
            std::fs::symlink_metadata(&resolved).is_ok_and(|meta| meta.file_type().is_symlink());
        if is_link {
            match dunce::canonicalize(&resolved) {
                Ok(target) if inside(&target) => {}
                _ => return Err(outside()),
            }
        }
        Ok(resolved)
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

/// Resolves every part of an absolute path that exists, symbolic links and `..` included, and
/// appends the parts that do not exist yet as they are.
fn resolve(path: &Path) -> Result<PathBuf, ErrorBody> {
    let mut existing = path.to_path_buf();
    // The parts that do not exist, the last one first.
    let mut missing: Vec<OsString> = Vec::new();
    loop {
        match dunce::canonicalize(&existing) {
            Ok(mut resolved) => {
                for part in missing.iter().rev() {
                    resolved.push(part);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                match existing.components().next_back() {
                    Some(Component::Normal(name)) => missing.push(name.to_owned()),
                    Some(Component::ParentDir) => {
                        return Err(ErrorBody::new(
                            ErrorCode::InvalidArgs,
                            format!(
                                "{} goes up with .. out of a directory that does not exist.",
                                path.display()
                            ),
                        )
                        .with_hint("Write the path without .., the way it will be once its directories exist."));
                    }
                    _ => {
                        return Err(ErrorBody::new(
                            ErrorCode::NotFound,
                            format!(
                                "{} is on a drive or volume that does not exist.",
                                path.display()
                            ),
                        ));
                    }
                }
                existing.pop();
            }
            Err(error) => {
                return Err(ErrorBody::new(
                    ErrorCode::Failed,
                    format!("{} cannot be resolved: {error}.", existing.display()),
                ));
            }
        }
    }
}

/// [`resolve`] for every part but the last, which is appended as given.
fn resolve_keeping_last(path: &Path) -> Result<PathBuf, ErrorBody> {
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => Ok(resolve(parent)?.join(name)),
        _ => resolve(path),
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
