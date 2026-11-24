//! Role capability analysis and plain-English descriptions shared by `legion pair`,
//! `device.roles`, and the owner-strength credential guards (`device.invite`, `device.set`,
//! `token.add`).

use legion_core::policy::{OWNER, Policy, Role, Rule};
use legion_core::proto::{Effect, ToolSpec};
use serde::{Deserialize, Serialize};

/// Plain-English explanation used whenever a credential tool or `legion pair` refuses or warns
/// about an owner-strength role (any role that allows an `admin` tool, or built-in `owner`).
pub const OWNER_STRENGTH_REASON: &str =
    "this role can change who may do what, which makes it as strong as owner";

/// Summary of one role a caller may assign to a newly paired device.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleSummary {
    /// Role name (`[roles.<name>]` in `policy.toml`, or built-in `owner`).
    pub name: String,
    /// Concise one-line plain-English description of what the role allows.
    pub description: String,
    /// Strongest known effect allowed by this role, or `null` when no known effect can be stated
    /// (empty allow list, or an allow rule matching no live tool on this node without a stronger
    /// known effect).
    pub strongest_effect: Option<Effect>,
    /// `true` when this role is `owner` or allows any `admin` tool (making it as strong as
    /// `owner`).
    pub owner_strength: bool,
}

struct RoleEffects {
    read: bool,
    write: bool,
    destroy: bool,
    exec: bool,
    power: bool,
    admin: bool,
    broad_write: bool,
    broad_exec: bool,
    broad_admin: bool,
    can_write_policy: bool,
    has_unknown_tool_rule: bool,
}

fn analyze_role(role: &Role, specs: &[ToolSpec]) -> RoleEffects {
    let broad_rule_allows = |effect: Effect| -> bool {
        let covers = |rule: &Rule| match rule {
            Rule::Effect(of) => *of == effect,
            Rule::Tools(pattern) => pattern.chars().all(|c| c == '*'),
        };
        !role.deny.iter().any(covers) && role.allow.iter().any(covers)
    };

    let spec_allowed = |spec: &ToolSpec| -> bool {
        !role.deny.iter().any(|rule| rule.matches(spec))
            && role.allow.iter().any(|rule| rule.matches(spec))
    };

    let allows_effect = |effect: Effect| -> bool {
        let mut saw_spec_of_effect = false;
        for spec in specs {
            if spec.effect == effect {
                saw_spec_of_effect = true;
                if spec_allowed(spec) {
                    return true;
                }
            }
        }
        if saw_spec_of_effect {
            false
        } else {
            broad_rule_allows(effect)
        }
    };

    // Any named or globbed allow rule (other than all-`*`) that matches no live `ToolSpec` has an
    // unknown effect (for example an extension defined only on another machine); treat it
    // conservatively so it is never described as "look only".
    let has_unknown_tool_rule = role.allow.iter().any(|rule| match rule {
        Rule::Effect(_) => false,
        Rule::Tools(pattern) => {
            if pattern.chars().all(|c| c == '*') {
                return false;
            }
            let explicitly_denied = role.deny.iter().any(|deny_rule| match deny_rule {
                Rule::Tools(dp) => dp == pattern || dp.chars().all(|c| c == '*'),
                Rule::Effect(_) => false,
            });
            if explicitly_denied {
                return false;
            }
            !specs.iter().any(|spec| rule.matches(spec))
        }
    });

    let can_write_policy = broad_rule_allows(Effect::Admin)
        || specs
            .iter()
            .find(|spec| spec.name == "config.write")
            .is_some_and(spec_allowed);

    RoleEffects {
        read: allows_effect(Effect::Read),
        write: allows_effect(Effect::Write),
        destroy: allows_effect(Effect::Destroy),
        exec: allows_effect(Effect::Exec),
        power: allows_effect(Effect::Power),
        admin: allows_effect(Effect::Admin),
        broad_write: broad_rule_allows(Effect::Write),
        broad_exec: broad_rule_allows(Effect::Exec),
        broad_admin: broad_rule_allows(Effect::Admin),
        can_write_policy,
        has_unknown_tool_rule,
    }
}

fn summarize_allow_rules(role: &Role) -> String {
    let allowed: Vec<String> = role.allow.iter().take(4).map(ToString::to_string).collect();
    let suffix = if role.allow.len() > 4 { ", ..." } else { "" };
    format!("{}{suffix}", allowed.join(", "))
}

/// Returns `true` when `name` is `owner` or `role` allows any `admin` tool (via `effect:admin`,
/// `*`, glob, or exact tool name) when resolved against `specs`.
pub fn is_owner_strength(name: &str, role: &Role, specs: &[ToolSpec]) -> bool {
    name == OWNER || analyze_role(role, specs).admin
}

/// Returns `true` when `role_name` in `policy` is `owner` or resolves to a role that allows any
/// `admin` tool against `specs`.
pub fn is_owner_strength_role(policy: &Policy, role_name: &str, specs: &[ToolSpec]) -> bool {
    if role_name == OWNER {
        return true;
    }
    policy
        .roles
        .get(role_name)
        .is_some_and(|role| is_owner_strength(role_name, role, specs))
}

/// Returns the strongest known effect allowed by `role` when resolved against `specs`, following
/// the same capability precedence as [`describe_role`] (`admin > exec > power > destroy > write >
/// read`), or `None` when nothing is allowed or an unknown tool rule prevents claiming a narrower
/// ceiling.
pub fn strongest_effect(name: &str, role: &Role, specs: &[ToolSpec]) -> Option<Effect> {
    if name == OWNER {
        return Some(Effect::Admin);
    }
    if role.allow.is_empty() {
        return None;
    }
    let eff = analyze_role(role, specs);
    if eff.admin {
        Some(Effect::Admin)
    } else if eff.exec {
        Some(Effect::Exec)
    } else if eff.power {
        Some(Effect::Power)
    } else if eff.destroy {
        Some(Effect::Destroy)
    } else if eff.has_unknown_tool_rule {
        None
    } else if eff.write {
        Some(Effect::Write)
    } else if eff.read {
        Some(Effect::Read)
    } else {
        None
    }
}

/// Returns a concise one-line plain-English description of what `role` may do, derived directly
/// from its rules in `policy.toml` resolved against the node's live `ToolSpec`s (with no
/// hardcoded role names).
pub fn describe_role(name: &str, role: &Role, specs: &[ToolSpec]) -> String {
    if name == OWNER {
        return "everything, including changing who may do what".to_owned();
    }
    if role.allow.is_empty() {
        return "nothing (no tools allowed)".to_owned();
    }

    let eff = analyze_role(role, specs);
    let summary = summarize_allow_rules(role);

    if eff.admin {
        if eff.broad_admin {
            "everything, including changing who may do what (admin — as strong as owner)".to_owned()
        } else if eff.can_write_policy {
            format!(
                "everything, including changing who may do what (allows admin — as strong as owner: {summary})"
            )
        } else {
            format!("custom (allows admin: {summary})")
        }
    } else if eff.exec {
        if eff.broad_exec && eff.write && !eff.has_unknown_tool_rule {
            "operate (read, change files, and run commands — as strong as this account)".to_owned()
        } else {
            format!("custom (allows exec — as strong as this account: {summary})")
        }
    } else if eff.power {
        format!("custom (allows power: {summary})")
    } else if eff.destroy {
        format!("custom (allows destroy: {summary})")
    } else if eff.has_unknown_tool_rule {
        format!("custom (unknown tool effect: {summary})")
    } else if eff.write {
        if eff.broad_write {
            "change files (read and edit files, no running commands)".to_owned()
        } else {
            format!("custom (allows write: {summary})")
        }
    } else if eff.read {
        "look only (read status and files, change nothing)".to_owned()
    } else {
        "nothing (no tools allowed)".to_owned()
    }
}

fn role_rank(name: &str, role: &Role, specs: &[ToolSpec]) -> u8 {
    if name == OWNER {
        return 7;
    }
    if role.allow.is_empty() {
        return 0;
    }
    let eff = analyze_role(role, specs);
    if eff.admin {
        6
    } else if eff.exec {
        5
    } else if eff.power {
        4
    } else if eff.destroy || eff.has_unknown_tool_rule {
        3
    } else if eff.write {
        2
    } else if eff.read {
        1
    } else {
        0
    }
}

/// Lists the policy's defined roles plus built-in `owner` ordered from narrowest to broadest, each
/// with its plain description, strongest effect, and `owner_strength` flag resolved against
/// `specs`.
pub fn role_summaries(policy: &Policy, specs: &[ToolSpec]) -> Vec<RoleSummary> {
    let mut items: Vec<(u8, RoleSummary)> = policy
        .roles
        .iter()
        .map(|(name, role)| {
            (
                role_rank(name, role, specs),
                RoleSummary {
                    name: name.clone(),
                    description: describe_role(name, role, specs),
                    strongest_effect: strongest_effect(name, role, specs),
                    owner_strength: is_owner_strength(name, role, specs),
                },
            )
        })
        .collect();
    items.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.name.cmp(&b.1.name)));
    let mut out: Vec<RoleSummary> = items.into_iter().map(|(_, summary)| summary).collect();
    out.push(RoleSummary {
        name: OWNER.to_owned(),
        description: describe_role(OWNER, &Role::default(), specs),
        strongest_effect: Some(Effect::Admin),
        owner_strength: true,
    });
    out
}

/// Lists the policy's defined roles plus built-in `owner` ordered from narrowest to broadest, each
/// paired with a plain one-line description resolved against `specs`.
pub fn role_choices(policy: &Policy, specs: &[ToolSpec]) -> Vec<(String, String)> {
    role_summaries(policy, specs)
        .into_iter()
        .map(|summary| (summary.name, summary.description))
        .collect()
}
