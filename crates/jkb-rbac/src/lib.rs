//! Role-based access control, declared as tables and decided by composable authorizers (design D52).
//!
//! The crate knows nothing about jkb. It has three ideas, each an abstraction a caller implements
//! for its own domain — the daemon's op set is one, a task workflow's events are another, and
//! neither is special:
//!
//! * **A [`RoleTable`] is data.** Which role holds which permission is a `&'static` table rather
//!   than a `match` spread over the call sites that ask, for the same reason `jkb-fsm` declares a
//!   lifecycle as a table: a table can be walked. [`Grants::check`] finds a role nothing grants
//!   anything to, a permission no role holds, and a role declared twice — the three ways a policy
//!   silently differs from what its author meant — and [`Grants::matrix`] renders the whole thing
//!   for a person to read. A table can also be built at runtime ([`OwnedTable`]) from a static base
//!   plus grants somebody chose, and answers the same questions.
//! * **An action says what it needs; a subject says what it holds.** [`Guarded::requirement`] and
//!   [`Principal::roles`] are the two halves, and [`RoleBased`] joins them against a table. A
//!   requirement can be [`Requirement::Nobody`] — an action no role may take through this door —
//!   which is different from a permission nobody was granted: it carries the sentence saying where
//!   the action *is* taken instead.
//! * **Authorizers compose.** [`Authorizer`] is one method, so a coarse rule (the op's class) and a
//!   fine one (this event on this task) are two authorizers joined by [`Both`], rather than one
//!   function that grows a branch per resource. [`AllowAll`] is the explicit form of *no policy*,
//!   so an unrestricted caller is a value somebody chose and not an `if` somebody forgot.
//!
//! A refusal ([`Refusal`]) names the roles that *would* have been allowed, so the caller can say
//! who to ask instead of only "no" — the same reason `jkb-fsm`'s denials carry a remedy.
//!
//! ```
//! use jkb_rbac::{Authorizer, Decision, Grant, Grants, Guarded, Permission, Principal, Requirement,
//!                Role, RoleBased, RoleTable};
//!
//! #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
//! enum Staff { Clerk, Manager }
//! impl Role for Staff {
//!     const ALL: &'static [Self] = &[Self::Clerk, Self::Manager];
//!     fn name(self) -> &'static str { match self { Self::Clerk => "clerk", Self::Manager => "manager" } }
//! }
//! #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
//! enum Till { Sell, Refund }
//! impl Permission for Till {
//!     const ALL: &'static [Self] = &[Self::Sell, Self::Refund];
//!     fn name(self) -> &'static str { match self { Self::Sell => "sell", Self::Refund => "refund" } }
//! }
//! static TABLE: RoleTable<Staff, Till> = RoleTable { grants: &[
//!     Grant { role: Staff::Clerk, permits: &[Till::Sell] },
//!     Grant { role: Staff::Manager, permits: &[Till::Sell, Till::Refund] },
//! ] };
//! assert!(TABLE.check().is_empty());
//!
//! struct Person(Staff);
//! impl Principal<Staff> for Person { fn roles(&self) -> Vec<Staff> { vec![self.0] } }
//! struct Ask(Till);
//! impl Guarded<Till> for Ask { fn requirement(&self) -> Requirement<Till> { Requirement::Permission(self.0) } }
//!
//! let policy = RoleBased::new(&TABLE);
//! assert!(policy.authorize(&Person(Staff::Clerk), &Ask(Till::Sell)).is_allowed());
//! let Decision::Deny(no) = policy.authorize(&Person(Staff::Clerk), &Ask(Till::Refund)) else { panic!() };
//! assert_eq!(no.allowed_roles, vec!["manager"]);
//! ```

use std::collections::HashSet;
use std::fmt;
use std::hash::Hash;

/// A role: a named bundle of permissions a subject can hold.
///
/// A closed set, declared by the domain, so a table can be checked for a role it forgot.
pub trait Role: Copy + Eq + Hash + fmt::Debug + 'static {
    /// Every role, so the table can be checked against the whole set.
    const ALL: &'static [Self];

    /// The stable name — what is stored, printed and parsed.
    fn name(self) -> &'static str;

    /// The role named `name`, if there is one.
    #[must_use]
    fn parse(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|r| r.name() == name)
    }
}

/// A permission: one thing a role may be allowed to do.
pub trait Permission: Copy + Eq + Hash + fmt::Debug + 'static {
    /// Every permission, so the table can be checked for one nobody holds.
    const ALL: &'static [Self];

    /// The stable name.
    fn name(self) -> &'static str;
}

/// One row of a [`RoleTable`]: a role and everything it may do.
#[derive(Debug)]
pub struct Grant<R: 'static, P: 'static> {
    /// The role.
    pub role: R,
    /// What it may do.
    pub permits: &'static [P],
}

/// Which role holds which permission, as data.
#[derive(Debug)]
pub struct RoleTable<R: 'static, P: 'static> {
    /// One row per role.
    pub grants: &'static [Grant<R, P>],
}

/// Something wrong with a [`RoleTable`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TableDefect<R, P> {
    /// Two rows for one role: which one wins would be decided by position.
    DuplicateRole(R),
    /// A role with no row, so it may do nothing — almost always a table somebody forgot to extend.
    UngrantedRole(R),
    /// A permission no role holds, so nothing that requires it can ever be done.
    UnheldPermission(P),
    /// A role lists one permission twice.
    RepeatedPermission(R, P),
}

impl<R: Role, P: Permission> fmt::Display for TableDefect<R, P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateRole(r) => write!(f, "role `{}` has two rows", r.name()),
            Self::UngrantedRole(r) => write!(f, "role `{}` has no row", r.name()),
            Self::UnheldPermission(p) => write!(f, "no role holds `{}`", p.name()),
            Self::RepeatedPermission(r, p) => {
                write!(f, "role `{}` lists `{}` twice", r.name(), p.name())
            }
        }
    }
}

/// Anything that answers *which role holds which permission* — the static [`RoleTable`] and the
/// runtime-built [`OwnedTable`] alike, so [`RoleBased`] decides over either and both get the same
/// [`Grants::check`] and [`Grants::matrix`].
pub trait Grants<R: Role, P: Permission> {
    /// Every row, in declaration order: a role and what it may do. A role may appear twice, which
    /// [`Grants::check`] reports.
    fn rows(&self) -> Vec<(R, Vec<P>)>;

    /// What `role` may do; nothing for a role with no row. The first row wins.
    fn permissions_of(&self, role: R) -> Vec<P> {
        self.rows()
            .into_iter()
            .find(|(r, _)| *r == role)
            .map(|(_, p)| p)
            .unwrap_or_default()
    }

    /// Whether `role` holds `permission`.
    fn permits(&self, role: R, permission: P) -> bool {
        self.permissions_of(role).contains(&permission)
    }

    /// Every role holding `permission`, in declaration order.
    fn roles_for(&self, permission: P) -> Vec<R> {
        self.rows()
            .into_iter()
            .filter(|(_, p)| p.contains(&permission))
            .map(|(r, _)| r)
            .collect()
    }

    /// Everything wrong with this table. Empty for a sound one; a test per table asserts that.
    fn check(&self) -> Vec<TableDefect<R, P>> {
        let mut defects = Vec::new();
        let mut seen = HashSet::new();
        for (role, permits) in self.rows() {
            if !seen.insert(role) {
                defects.push(TableDefect::DuplicateRole(role));
            }
            let mut listed = HashSet::new();
            for p in permits {
                if !listed.insert(p) {
                    defects.push(TableDefect::RepeatedPermission(role, p));
                }
            }
        }
        for &r in R::ALL {
            if !seen.contains(&r) {
                defects.push(TableDefect::UngrantedRole(r));
            }
        }
        for &p in P::ALL {
            if self.roles_for(p).is_empty() {
                defects.push(TableDefect::UnheldPermission(p));
            }
        }
        defects
    }

    /// The table as Markdown — one row per permission, one column per role — for `jkb role matrix`
    /// and for a design doc that must not drift from the code.
    fn matrix(&self) -> String {
        let mut out = String::from("| permission |");
        for &r in R::ALL {
            out.push(' ');
            out.push_str(r.name());
            out.push_str(" |");
        }
        out.push_str("\n|---|");
        for _ in R::ALL {
            out.push_str("---|");
        }
        for &p in P::ALL {
            out.push_str("\n| ");
            out.push_str(p.name());
            out.push_str(" |");
            for &r in R::ALL {
                out.push_str(if self.permits(r, p) { " ✓ |" } else { "   |" });
            }
        }
        out.push('\n');
        out
    }
}

impl<R: Role, P: Permission> Grants<R, P> for RoleTable<R, P> {
    fn rows(&self) -> Vec<(R, Vec<P>)> {
        self.grants
            .iter()
            .map(|g| (g.role, g.permits.to_vec()))
            .collect()
    }
}

/// A role table built at runtime: a static base plus grants somebody chose — a workflow strategy's
/// permission toggles, say. Same questions, same checks, as [`RoleTable`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedTable<R, P> {
    rows: Vec<(R, Vec<P>)>,
}

impl<R: Role, P: Permission> OwnedTable<R, P> {
    /// A copy of `base` to extend.
    #[must_use]
    pub fn from_table(base: &impl Grants<R, P>) -> Self {
        Self { rows: base.rows() }
    }

    /// Also let `role` do `permission`. Idempotent; a role with no row gains one.
    #[must_use]
    pub fn grant(mut self, role: R, permission: P) -> Self {
        match self.rows.iter_mut().find(|(r, _)| *r == role) {
            Some((_, permits)) => {
                if !permits.contains(&permission) {
                    permits.push(permission);
                }
            }
            None => self.rows.push((role, vec![permission])),
        }
        self
    }

    /// No longer let `role` do `permission`.
    #[must_use]
    pub fn revoke(mut self, role: R, permission: P) -> Self {
        if let Some((_, permits)) = self.rows.iter_mut().find(|(r, _)| *r == role) {
            permits.retain(|p| *p != permission);
        }
        self
    }
}

impl<R: Role, P: Permission> Grants<R, P> for OwnedTable<R, P> {
    fn rows(&self) -> Vec<(R, Vec<P>)> {
        self.rows.clone()
    }
}

/// A subject that holds roles.
pub trait Principal<R: Role> {
    /// The roles held. Holding any one that permits an action is enough.
    fn roles(&self) -> Vec<R>;
}

/// What an action needs before it may be taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Requirement<P> {
    /// Anyone who reached this point may take it.
    Anyone,
    /// A role holding this permission.
    Permission(P),
    /// Nobody through this door; the sentence says where it is done instead.
    Nobody(&'static str),
}

/// An action that says what it needs.
pub trait Guarded<P: Permission> {
    /// The requirement.
    fn requirement(&self) -> Requirement<P>;
}

/// Why an action was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// A complete sentence.
    pub reason: String,
    /// The roles that would have been allowed — who to ask instead. Empty when none would.
    pub allowed_roles: Vec<&'static str>,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.reason)?;
        if !self.allowed_roles.is_empty() {
            write!(f, " (allowed: {})", self.allowed_roles.join(", "))?;
        }
        Ok(())
    }
}

/// An authorization decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Go ahead.
    Allow,
    /// Refused, and why.
    Deny(Refusal),
}

impl Decision {
    /// A denial with no role that would have been allowed.
    #[must_use]
    pub fn deny(reason: impl Into<String>) -> Self {
        Self::Deny(Refusal {
            reason: reason.into(),
            allowed_roles: Vec::new(),
        })
    }

    /// Whether it allows.
    #[must_use]
    pub const fn is_allowed(&self) -> bool {
        matches!(self, Self::Allow)
    }

    /// `Ok(())` or the refusal, for `?`.
    ///
    /// # Errors
    /// The [`Refusal`] when it denies.
    pub fn into_result(self) -> Result<(), Refusal> {
        match self {
            Self::Allow => Ok(()),
            Self::Deny(r) => Err(r),
        }
    }
}

/// Decides whether `subject` may take `action`.
pub trait Authorizer<S: ?Sized, A: ?Sized> {
    /// The decision.
    fn authorize(&self, subject: &S, action: &A) -> Decision;
}

/// No policy: everything is allowed. The explicit spelling, so an unrestricted caller is chosen.
#[derive(Debug, Clone, Copy, Default)]
pub struct AllowAll;

impl<S: ?Sized, A: ?Sized> Authorizer<S, A> for AllowAll {
    fn authorize(&self, _: &S, _: &A) -> Decision {
        Decision::Allow
    }
}

/// Both authorizers must allow; the first refusal is the answer.
#[derive(Debug, Clone, Copy)]
pub struct Both<X, Y>(pub X, pub Y);

impl<S: ?Sized, A: ?Sized, X: Authorizer<S, A>, Y: Authorizer<S, A>> Authorizer<S, A>
    for Both<X, Y>
{
    fn authorize(&self, subject: &S, action: &A) -> Decision {
        match self.0.authorize(subject, action) {
            Decision::Allow => self.1.authorize(subject, action),
            deny @ Decision::Deny(_) => deny,
        }
    }
}

/// An authorizer written as a function, for a rule that is about the resource rather than the role.
#[derive(Clone, Copy)]
pub struct FnAuthorizer<F>(pub F);

impl<S: ?Sized, A: ?Sized, F: Fn(&S, &A) -> Decision> Authorizer<S, A> for FnAuthorizer<F> {
    fn authorize(&self, subject: &S, action: &A) -> Decision {
        (self.0)(subject, action)
    }
}

/// The role-based authorizer: a subject may take an action when one of its roles holds the
/// permission the action requires, per a table ([`RoleTable`] or [`OwnedTable`]).
pub struct RoleBased<'t, R, P> {
    table: &'t dyn Grants<R, P>,
}

impl<R, P> Clone for RoleBased<'_, R, P> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<R, P> Copy for RoleBased<'_, R, P> {}

impl<'t, R: Role, P: Permission> RoleBased<'t, R, P> {
    /// An authorizer over `table`.
    #[must_use]
    pub fn new(table: &'t dyn Grants<R, P>) -> Self {
        Self { table }
    }

    /// The decision for a subject holding `roles` asking for `requirement` — the core of
    /// [`Authorizer::authorize`], for a caller that has the roles and the requirement in hand.
    #[must_use]
    pub fn decide(&self, roles: &[R], requirement: Requirement<P>) -> Decision {
        match requirement {
            Requirement::Anyone => Decision::Allow,
            Requirement::Nobody(why) => Decision::deny(why),
            Requirement::Permission(p) => {
                if roles.iter().any(|&r| self.table.permits(r, p)) {
                    return Decision::Allow;
                }
                let held: Vec<&str> = roles.iter().map(|r| r.name()).collect();
                Decision::Deny(Refusal {
                    reason: format!(
                        "`{}` needs a role that holds it, and this caller holds {}",
                        p.name(),
                        if held.is_empty() {
                            "no role".to_owned()
                        } else {
                            held.join(", ")
                        }
                    ),
                    allowed_roles: self.table.roles_for(p).iter().map(|r| r.name()).collect(),
                })
            }
        }
    }
}

impl<R: Role, P: Permission, S: Principal<R> + ?Sized, A: Guarded<P> + ?Sized> Authorizer<S, A>
    for RoleBased<'_, R, P>
{
    fn authorize(&self, subject: &S, action: &A) -> Decision {
        self.decide(&subject.roles(), action.requirement())
    }
}

#[cfg(test)]
mod tests;
