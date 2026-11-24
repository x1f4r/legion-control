//! Shared principal-scoping rules for credential-management tools (`token.*` and `device.*`).
//!
//! A caller may only create, inspect or revoke credentials for a principal it could itself act as:
//! its own principal, or any principal when the caller has the `owner` role. Credentials for a
//! principal with the `owner` role additionally require explicit `allow_owner = true` so full
//! account access is never handed out by accident.

use std::sync::Arc;

use legion_core::Ctx;
use legion_core::policy::{OWNER, Policy};
use legion_core::proto::{ErrorBody, ErrorCode};

/// Who is calling a credential tool, as far as deciding whose credentials it may manage goes.
pub(crate) struct Caller {
    principal: String,
    /// Whether the caller's role is the owner role, the one role that may act as anyone.
    owner: bool,
    policy: Arc<Policy>,
}

impl Caller {
    /// Loads the caller's identity and live policy from `ctx`.
    pub(crate) fn of(ctx: &Ctx) -> Result<Caller, ErrorBody> {
        Ok(Caller {
            principal: ctx.principal().to_owned(),
            owner: ctx.grant().role() == OWNER,
            policy: ctx.config().get::<Policy>()?,
        })
    }

    /// Whether the caller itself holds the `owner` role.
    pub(crate) fn is_owner(&self) -> bool {
        self.owner
    }

    /// Whether `principal` has the owner role, by name (`owner`) or by the role the file gives it.
    pub(crate) fn has_owner_role(&self, principal: &str) -> bool {
        self.policy.role_of(principal).0 == OWNER
    }

    /// Whether the caller may make, see or remove a credential for `principal`: one it could act as
    /// itself, so a credential never gives anyone more than they already have. That is its own
    /// principal, or any when the caller has the owner role, which alone may act as others; and a
    /// credential for a principal with the owner role, the keys to the machine, only a caller with
    /// the owner role may handle.
    pub(crate) fn may_manage(&self, principal: &str) -> bool {
        if self.has_owner_role(principal) {
            self.owner
        } else {
            self.owner || principal == self.principal
        }
    }

    /// Checks whether the caller may issue a new credential (token or device invite) for
    /// `principal`, enforcing:
    /// 1. `may_manage(principal)` (caller scoping),
    /// 2. `allow_owner` guard when `principal` has the `owner` role, and
    /// 3. `principal` is named in `policy.toml` when it does not have the `owner` role.
    ///
    /// Returns the resolved role name of `principal` on success.
    pub(crate) fn check_issue(
        &self,
        principal: &str,
        allow_owner: bool,
        noun: &str,
        owner_message: String,
        owner_hint: &str,
    ) -> Result<String, ErrorBody> {
        if let Some(owner_id) = self.policy.device_owner(principal) {
            return Err(ErrorBody::new(
                ErrorCode::Denied,
                format!(
                    "{principal} is owned by paired device {owner_id}, so {noun} cannot be created on it."
                ),
            )
            .with_hint(
                "Create a separate principal in policy.toml for shared tokens or `--as` pairings, or pair a new device with `legion pair --name ... --role ...`.",
            ));
        }

        if !self.may_manage(principal) {
            let message = if self.has_owner_role(principal) {
                format!(
                    "{} may not make {noun} for {principal}: it has the owner role, and only a caller with the owner role may hand out {noun} that may do everything the account can.",
                    self.principal
                )
            } else {
                format!(
                    "{} may not make {noun} for {principal}: {noun} may stand only for the caller itself, unless the caller has the owner role.",
                    self.principal
                )
            };
            return Err(ErrorBody::new(ErrorCode::Denied, message)
                .with_hint(format!("Ask the owner of this machine to make {noun}.")));
        }

        if self.has_owner_role(principal) {
            if !allow_owner {
                return Err(ErrorBody::new(ErrorCode::Denied, owner_message).with_hint(owner_hint));
            }
            return Ok(OWNER.to_owned());
        }

        if !self
            .policy
            .principals
            .keys()
            .any(|known| known.as_str() == principal)
        {
            return Err(ErrorBody::new(
                ErrorCode::NotFound,
                format!(
                    "policy.toml does not name {principal}, so {noun} for it could do nothing."
                ),
            )
            .with_hint(format!(
                "Add [principals.\"{principal}\"] with a role to policy.toml first."
            )));
        }

        Ok(self.policy.role_of(principal).0)
    }
}
