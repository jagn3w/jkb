//! A strategy: an execution graph, permission toggles and attributes (design D52.5).
//!
//! The operator's clarification is the shape of this module: *who lands is set per strategy, but a
//! strategy can have an execution graph, and permission toggles, maybe other attributes in the
//! future.* So a [`StrategySpec`] composes three independent parts, and named strategies are
//! [`presets`](PRESETS) over it rather than hand-written tables:
//!
//! * **the graph** — [`GraphId`], a checkable `&'static` table;
//! * **toggles** — [`Toggles`], each a grant the operator chooses, bounded by a [domain](TOGGLES)
//!   so no toggle can hand an operator-only power to a worker;
//! * **attributes** — [`Attributes`], knobs the graph's guards read.
//!
//! A spec is serialized with `deny_unknown_fields` and every field defaulted: adding a toggle or an
//! attribute is additive, and an older binary that meets one it does not know **refuses** the task
//! rather than ignoring it — an ignored toggle could only ever mean a laxer policy than was chosen.

use jkb_rbac::{Decision, Grant, Grants as _, OwnedTable, Requirement, RoleBased, RoleTable};
use serde::{Deserialize, Serialize};

use super::{GraphId, Phase, WorkflowEvent};
use crate::roles::Role;
use crate::{Error, Result};

impl Serialize for Role {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Role {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let name = String::deserialize(d)?;
        Self::parse(&name).map_err(serde::de::Error::custom)
    }
}

/// The grants the operator chooses. Each lists roles **beyond the operator**, who always holds
/// everything; an empty list means the operator alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Toggles {
    /// Who may `approve_design` / `reject_design`.
    pub approves_design: Vec<Role>,
    /// Who may land the task (the `task.land` op).
    pub lands: Vec<Role>,
}

impl Default for Toggles {
    fn default() -> Self {
        Self {
            approves_design: vec![Role::Operator],
            lands: vec![Role::Operator],
        }
    }
}

/// What a toggle controls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Controls {
    /// These workflow events.
    Events(&'static [WorkflowEvent]),
    /// Landing — an op, not a workflow event.
    Land,
}

/// A toggle's declaration: its name, what it controls, which roles it may ever name, and the phase
/// it acts in (a toggle whose graph never reaches that phase says nothing, and is refused).
#[derive(Debug, Clone, Copy)]
pub struct ToggleDef {
    /// The spec field and CLI flag.
    pub name: &'static str,
    /// What it grants.
    pub controls: Controls,
    /// The roles it may name.
    pub domain: &'static [Role],
    /// The phase it acts in, if it acts in only one.
    pub acts_in: Option<Phase>,
}

/// Every toggle. A new one is a row here, a field in [`Toggles`], an arm in [`Toggles::get`], and a
/// test; nothing that asks a permission changes.
pub const TOGGLES: &[ToggleDef] = &[
    ToggleDef {
        name: "approves_design",
        controls: Controls::Events(&[WorkflowEvent::ApproveDesign, WorkflowEvent::RejectDesign]),
        domain: &[Role::Operator, Role::Coordinator],
        acts_in: Some(Phase::DesignReview),
    },
    ToggleDef {
        name: "lands",
        controls: Controls::Land,
        domain: &[Role::Operator, Role::Coordinator],
        acts_in: None,
    },
];

impl Toggles {
    /// The roles the toggle named `name` lists.
    #[must_use]
    pub fn get(&self, name: &str) -> &[Role] {
        match name {
            "approves_design" => &self.approves_design,
            "lands" => &self.lands,
            _ => &[],
        }
    }

    /// Set the toggle named `name`.
    ///
    /// # Errors
    /// [`Error::Types`] for a toggle that does not exist.
    pub fn set(&mut self, name: &str, roles: Vec<Role>) -> Result<()> {
        match name {
            "approves_design" => self.approves_design = roles,
            "lands" => self.lands = roles,
            _ => {
                return Err(invalid(format!(
                    "no toggle `{name}` (toggles: {})",
                    TOGGLES
                        .iter()
                        .map(|t| t.name)
                        .collect::<Vec<_>>()
                        .join(", ")
                )))
            }
        }
        Ok(())
    }
}

/// What counts as "the same area" for [`RepeatedArea`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AreaScope {
    /// The same file.
    File,
    /// The same directory.
    Directory,
}

/// When must-fixes count as repeating (operator decision 3: the same file in two consecutive
/// rounds).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepeatedArea {
    /// The granularity of an area.
    pub scope: AreaScope,
    /// How many consecutive rounds must share one.
    pub rounds: u32,
}

impl Default for RepeatedArea {
    fn default() -> Self {
        Self {
            scope: AreaScope::File,
            rounds: 2,
        }
    }
}

/// The most consecutive rounds `repeated_area` may ask for.
pub const MAX_REPEATED_ROUNDS: u32 = 10;

/// Knobs the graph's guards read.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Attributes {
    /// When review findings count as repeating an area.
    pub repeated_area: RepeatedArea,
}

/// A strategy, as stored: which graph, which toggles, which attributes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StrategySpec {
    /// The execution graph.
    pub graph: GraphId,
    /// The operator's grants.
    #[serde(default)]
    pub toggles: Toggles,
    /// The guards' knobs.
    #[serde(default)]
    pub attributes: Attributes,
}

/// The preset every task runs when nothing else is set (operator decision 1).
pub const DEFAULT_PRESET: &str = "design-reviewed";

/// A named preset.
pub struct Preset {
    /// Its name.
    pub name: &'static str,
    /// One line.
    pub describe: &'static str,
    /// Its spec.
    pub spec: fn() -> StrategySpec,
}

/// The presets.
pub const PRESETS: &[Preset] = &[
    Preset {
        name: "design-reviewed",
        describe: "the operator approves each design and lands (the default)",
        spec: || StrategySpec {
            graph: GraphId::ReviewedDesign,
            toggles: Toggles::default(),
            attributes: Attributes::default(),
        },
    },
    Preset {
        name: "coordinated",
        describe: "the coordinator may approve designs; the operator lands",
        spec: || StrategySpec {
            graph: GraphId::ReviewedDesign,
            toggles: Toggles {
                approves_design: vec![Role::Operator, Role::Coordinator],
                lands: vec![Role::Operator],
            },
            attributes: Attributes::default(),
        },
    },
    Preset {
        name: "autonomous",
        describe: "no design review; the coordinator may land",
        spec: || StrategySpec {
            graph: GraphId::Direct,
            toggles: Toggles {
                approves_design: vec![Role::Operator],
                lands: vec![Role::Operator, Role::Coordinator],
            },
            attributes: Attributes::default(),
        },
    },
];

/// The preset named `name`.
#[must_use]
pub fn preset(name: &str) -> Option<StrategySpec> {
    PRESETS.iter().find(|p| p.name == name).map(|p| (p.spec)())
}

/// The grants every strategy shares — the ones that are not a choice. Toggles add to this.
pub static BASE: RoleTable<Role, WorkflowEvent> = RoleTable {
    grants: &[
        Grant {
            role: Role::Operator,
            permits: <WorkflowEvent as jkb_fsm::Event>::ALL,
        },
        Grant {
            role: Role::Coordinator,
            permits: &[
                WorkflowEvent::SubmitDesign,
                WorkflowEvent::SubmitWork,
                WorkflowEvent::SubmitSystemic,
                WorkflowEvent::SystemicRedesign,
                WorkflowEvent::Rework,
                WorkflowEvent::Cancel,
            ],
        },
        Grant {
            role: Role::Designer,
            permits: &[WorkflowEvent::SubmitDesign],
        },
        Grant {
            role: Role::Implementer,
            permits: &[WorkflowEvent::SubmitWork],
        },
        Grant {
            role: Role::Reviewer,
            permits: &[],
        },
        Grant {
            role: Role::SystemicReviewer,
            permits: &[
                WorkflowEvent::SubmitSystemic,
                WorkflowEvent::SystemicRedesign,
            ],
        },
    ],
};

fn invalid(msg: impl Into<String>) -> Error {
    Error::Types(jkb_types::Error::Validation(msg.into()))
}

impl StrategySpec {
    /// Refuse a spec that could not mean what it says (D52.5): a toggle naming a role outside its
    /// domain, a toggle whose graph never reaches the phase it acts in, an attribute out of range,
    /// or a resulting permission table with a defect.
    ///
    /// # Errors
    /// [`Error::Types`] naming the first problem.
    pub fn validate(&self) -> Result<()> {
        for t in TOGGLES {
            let roles = self.toggles.get(t.name);
            if let Some(r) = roles.iter().find(|r| !t.domain.contains(r)) {
                return Err(invalid(format!(
                    "toggle `{}` may name only {} (not {})",
                    t.name,
                    t.domain
                        .iter()
                        .map(|r| r.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                    r.as_str()
                )));
            }
            if let Some(phase) = t.acts_in {
                let beyond_operator = roles.iter().any(|r| *r != Role::Operator);
                if beyond_operator && !self.graph.reaches_unforced(phase) {
                    return Err(invalid(format!(
                        "toggle `{}` acts in `{}`, which the `{}` graph reaches only by an operator \
                         override — it would say nothing",
                        t.name,
                        phase.as_str(),
                        self.graph.as_str()
                    )));
                }
            }
        }
        let rounds = self.attributes.repeated_area.rounds;
        if !(2..=MAX_REPEATED_ROUNDS).contains(&rounds) {
            return Err(invalid(format!(
                "repeated_area.rounds of 2 to {MAX_REPEATED_ROUNDS} (got {rounds})"
            )));
        }
        let defects = self.permissions().check();
        if let Some(d) = defects.first() {
            return Err(invalid(format!("the strategy's permission table: {d}")));
        }
        Ok(())
    }

    /// Parse and validate a stored spec. An unknown field is refused — see the module doc.
    ///
    /// # Errors
    /// [`Error::Types`] for a spec this binary cannot read, or one that does not validate.
    pub fn from_json(json: &str) -> Result<Self> {
        let spec: Self = serde_json::from_str(json).map_err(|e| {
            invalid(format!(
                "this task's workflow strategy cannot be read by this jkb ({e}) — a newer jkb may \
                 have set it"
            ))
        })?;
        spec.validate()?;
        Ok(spec)
    }

    /// The stored form.
    ///
    /// # Errors
    /// Never in practice; serialization of this type cannot fail.
    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string(self).map_err(|e| invalid(e.to_string()))
    }

    /// Who may fire which event: [`BASE`] plus the toggles.
    #[must_use]
    pub fn permissions(&self) -> OwnedTable<Role, WorkflowEvent> {
        let mut table = OwnedTable::from_table(&BASE);
        for t in TOGGLES {
            if let Controls::Events(events) = t.controls {
                for &role in self.toggles.get(t.name) {
                    for &e in events {
                        table = table.grant(role, e);
                    }
                }
            }
        }
        table
    }

    /// What firing `event` requires: observations are anyone's (their guards decide), everything
    /// else a role the effective table grants it to.
    #[must_use]
    pub fn requirement(event: WorkflowEvent) -> Requirement<WorkflowEvent> {
        match jkb_fsm::Event::kind(event) {
            jkb_fsm::EventKind::Reconciled => Requirement::Anyone,
            jkb_fsm::EventKind::Applied => Requirement::Permission(event),
        }
    }

    /// May a holder of `roles` fire `event` under this strategy?
    #[must_use]
    pub fn authorize(&self, roles: &[Role], event: WorkflowEvent) -> Decision {
        let table = self.permissions();
        RoleBased::new(&table).decide(roles, Self::requirement(event))
    }

    /// May a holder of `roles` land a task under this strategy? The operator always may.
    #[must_use]
    pub fn may_land(&self, roles: &[Role]) -> Decision {
        if roles
            .iter()
            .any(|r| *r == Role::Operator || self.toggles.lands.contains(r))
        {
            return Decision::Allow;
        }
        let mut allowed: Vec<&'static str> = vec![Role::Operator.as_str()];
        allowed.extend(
            self.toggles
                .lands
                .iter()
                .filter(|r| **r != Role::Operator)
                .map(|r| r.as_str()),
        );
        Decision::Deny(jkb_rbac::Refusal {
            reason: "this task's strategy does not let this caller land it".to_owned(),
            allowed_roles: allowed,
        })
    }

    /// Who acts next at `phase`, and what they do — the answer `jkb workflow next` gives.
    #[must_use]
    pub fn next_actor(&self, phase: Phase) -> (Role, &'static str) {
        let first_non_operator = |roles: &[Role]| {
            roles
                .iter()
                .copied()
                .find(|r| *r != Role::Operator)
                .unwrap_or(Role::Operator)
        };
        match phase {
            Phase::Design => (
                Role::Designer,
                "write the design into the task, then `jkb workflow fire <uid> submit_design`",
            ),
            Phase::DesignReview => (
                first_non_operator(&self.toggles.approves_design),
                "read the design, then `jkb workflow fire <uid> approve_design` (or \
                 `reject_design`)",
            ),
            Phase::Implement => (
                Role::Implementer,
                "implement and commit, then `jkb workflow fire <uid> submit_work`",
            ),
            Phase::Review => (
                Role::Reviewer,
                "run a review round (`/jkb-review`), then `jkb workflow observe <uid>`",
            ),
            Phase::SystemicReview => (
                Role::SystemicReviewer,
                "find the systemic cause and record it, then `jkb workflow fire <uid> \
                 submit_systemic --reason …` — or `systemic_redesign --reason …` if the fix \
                 changes how the operator understands the system",
            ),
            Phase::Landable => (
                first_non_operator(&self.toggles.lands),
                "land it: `jkb task land <uid>`",
            ),
            Phase::Landed | Phase::Cancelled => (Role::Operator, "nothing: the task is settled"),
        }
    }
}
