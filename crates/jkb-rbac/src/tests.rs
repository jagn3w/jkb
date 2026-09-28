use super::{
    AllowAll, Authorizer, Both, Decision, FnAuthorizer, Grant, Grants, Guarded, OwnedTable,
    Permission, Principal, Requirement, Role, RoleBased, RoleTable, TableDefect,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum R {
    A,
    B,
    C,
}
impl Role for R {
    const ALL: &'static [Self] = &[Self::A, Self::B, Self::C];
    fn name(self) -> &'static str {
        match self {
            Self::A => "a",
            Self::B => "b",
            Self::C => "c",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum P {
    Read,
    Write,
    Admin,
}
impl Permission for P {
    const ALL: &'static [Self] = &[Self::Read, Self::Write, Self::Admin];
    fn name(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Admin => "admin",
        }
    }
}

static SOUND: RoleTable<R, P> = RoleTable {
    grants: &[
        Grant {
            role: R::A,
            permits: &[P::Read],
        },
        Grant {
            role: R::B,
            permits: &[P::Read, P::Write],
        },
        Grant {
            role: R::C,
            permits: &[P::Read, P::Write, P::Admin],
        },
    ],
};

struct Holds(Vec<R>);
impl Principal<R> for Holds {
    fn roles(&self) -> Vec<R> {
        self.0.clone()
    }
}
struct Needs(Requirement<P>);
impl Guarded<P> for Needs {
    fn requirement(&self) -> Requirement<P> {
        self.0
    }
}

#[test]
fn a_sound_table_has_no_defects() {
    assert_eq!(SOUND.check(), vec![]);
    assert!(SOUND.permits(R::B, P::Write));
    assert!(!SOUND.permits(R::A, P::Write));
    assert_eq!(SOUND.roles_for(P::Admin), vec![R::C]);
}

#[test]
fn every_defect_shape_is_found() {
    static BAD: RoleTable<R, P> = RoleTable {
        grants: &[
            Grant {
                role: R::A,
                permits: &[P::Read, P::Read],
            },
            Grant {
                role: R::A,
                permits: &[],
            },
        ],
    };
    let d = BAD.check();
    assert!(d.contains(&TableDefect::DuplicateRole(R::A)));
    assert!(d.contains(&TableDefect::RepeatedPermission(R::A, P::Read)));
    assert!(d.contains(&TableDefect::UngrantedRole(R::B)));
    assert!(d.contains(&TableDefect::UngrantedRole(R::C)));
    assert!(d.contains(&TableDefect::UnheldPermission(P::Write)));
    assert!(d.contains(&TableDefect::UnheldPermission(P::Admin)));
    assert_eq!(d.len(), 6, "{d:?}");
}

#[test]
fn any_held_role_that_permits_is_enough_and_a_refusal_names_who_may() {
    let policy = RoleBased::new(&SOUND);
    let write = Needs(Requirement::Permission(P::Write));
    assert!(policy
        .authorize(&Holds(vec![R::A, R::B]), &write)
        .is_allowed());
    let Decision::Deny(no) = policy.authorize(&Holds(vec![R::A]), &write) else {
        panic!("a reader may not write")
    };
    assert_eq!(no.allowed_roles, vec!["b", "c"]);
    assert!(no.reason.contains("holds a"), "{}", no.reason);
    let Decision::Deny(none) = policy.authorize(&Holds(vec![]), &write) else {
        panic!("no role, no write")
    };
    assert!(none.reason.contains("no role"), "{}", none.reason);
}

#[test]
fn anyone_and_nobody_do_not_consult_the_table() {
    let policy = RoleBased::new(&SOUND);
    assert!(policy
        .authorize(&Holds(vec![]), &Needs(Requirement::Anyone))
        .is_allowed());
    let Decision::Deny(no) = policy.authorize(
        &Holds(vec![R::C]),
        &Needs(Requirement::Nobody("done on the host")),
    ) else {
        panic!("nobody means nobody, even the most privileged role")
    };
    assert_eq!(no.reason, "done on the host");
    assert!(no.allowed_roles.is_empty());
}

#[test]
fn both_needs_both_and_reports_the_first_refusal() {
    let only_b = FnAuthorizer(|s: &Holds, _: &Needs| {
        if s.0.contains(&R::B) {
            Decision::Allow
        } else {
            Decision::deny("not b")
        }
    });
    let both = Both(RoleBased::new(&SOUND), only_b);
    let read = Needs(Requirement::Permission(P::Read));
    assert!(both.authorize(&Holds(vec![R::B]), &read).is_allowed());
    assert_eq!(
        both.authorize(&Holds(vec![R::C]), &read),
        Decision::deny("not b")
    );
    let write = Needs(Requirement::Permission(P::Write));
    let Decision::Deny(first) = both.authorize(&Holds(vec![R::A]), &write) else {
        panic!()
    };
    assert_eq!(
        first.allowed_roles,
        vec!["b", "c"],
        "the table refused first"
    );
    assert!(AllowAll.authorize(&Holds(vec![]), &write).is_allowed());
}

#[test]
fn the_matrix_has_a_row_per_permission_and_a_mark_per_grant() {
    let m = SOUND.matrix();
    assert!(m.starts_with("| permission | a | b | c |"), "{m}");
    assert!(m.contains("| admin |   |   | ✓ |"), "{m}");
    assert_eq!(m.matches('✓').count(), 6);
}

#[test]
fn roles_parse_by_name() {
    assert_eq!(R::parse("b"), Some(R::B));
    assert_eq!(R::parse("B"), None, "names are exact");
}

#[test]
fn an_owned_table_extends_a_static_base_and_is_checked_the_same_way() {
    let base = RoleTable {
        grants: &[
            Grant {
                role: R::A,
                permits: &[P::Read],
            },
            Grant {
                role: R::B,
                permits: &[P::Read, P::Write],
            },
        ],
    };
    // The static base alone has gaps an owned table can fill.
    assert!(base.check().contains(&TableDefect::UngrantedRole(R::C)));
    let owned = OwnedTable::from_table(&base)
        .grant(R::A, P::Write)
        .grant(R::A, P::Write)
        .grant(R::C, P::Admin)
        .revoke(R::B, P::Write);
    assert!(owned.permits(R::A, P::Write), "granted");
    assert!(!owned.permits(R::B, P::Write), "revoked");
    assert_eq!(
        owned.permissions_of(R::A),
        vec![P::Read, P::Write],
        "idempotent"
    );
    assert_eq!(
        owned.check(),
        vec![],
        "C gained a row, Admin gained a holder"
    );
    // And an authorizer decides over it exactly as over a static table.
    let policy = RoleBased::new(&owned);
    let Decision::Deny(no) = policy.authorize(
        &Holds(vec![R::B]),
        &Needs(Requirement::Permission(P::Admin)),
    ) else {
        panic!("b holds no admin")
    };
    assert_eq!(no.allowed_roles, vec!["c"]);
}
