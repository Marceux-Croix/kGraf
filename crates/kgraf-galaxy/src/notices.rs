//! Where notices come from (decision 0068): the plan overlay's work nodes, one notice per
//! thing that needs the user, counted once, on its work node.
//!
//! - A failure (a gate that failed or holds the work below, a schedule that stopped
//!   itself) is one [`NoticeKind::Failed`].
//! - A node held for the user is one [`NoticeKind::NeedsInput`], and each waiting
//!   question or approval ([`PlanNode::asks`]) is one more.
//! - Work in flight is one [`NoticeKind::Working`], kept as data: it is not counted and
//!   not drawn. Leases add nothing: one question on a task holding 30 files is one
//!   notice, not 32. The agent circles and their leases already show activity.
//! - New results are not produced here; they belong to the project dot and the live
//!   activity card (decision 0063), so nothing here depends on when they vanish.
//!
//! [`crate::fold::aggregate`] then stacks them on whatever is drawn.

use crate::fold::{Detail, Notice, NoticeKind};
use crate::plan::{PlanNode, WorkState};
use crate::project::ProjectModel;

impl NoticeKind {
    /// The notice a work node in `state` carries, if any. Waiting states (blocked, held
    /// by the cap, colliding, waiting) are not notices: they are neutral gray with the
    /// reason in words, never orange.
    pub fn of_state(state: WorkState) -> Option<NoticeKind> {
        match state {
            WorkState::GateHolding | WorkState::ScheduleDisabled => Some(NoticeKind::Failed),
            WorkState::HeldForUser => Some(NoticeKind::NeedsInput),
            WorkState::InFlight | WorkState::GateRunning => Some(NoticeKind::Working),
            WorkState::Done
            | WorkState::GatePassed
            | WorkState::Blocked
            | WorkState::HeldByCap
            | WorkState::Colliding
            | WorkState::Waiting
            | WorkState::Planned
            | WorkState::GatePending
            | WorkState::ScheduleOff
            | WorkState::Split
            | WorkState::PresumedDone
            | WorkState::PresumedPartial
            | WorkState::PresumedRemaining => None,
        }
    }
}

/// The notices of one plan node drawn at `slot`.
pub fn notices_of(node: &PlanNode, slot: u32) -> impl Iterator<Item = Notice> + '_ {
    let own = NoticeKind::of_state(node.state).map(|kind| {
        let detail = match node.state {
            WorkState::GateHolding => Detail::FailedGate,
            WorkState::ScheduleDisabled => Detail::StoppedSchedule,
            WorkState::HeldForUser => Detail::HeldForYou,
            _ => Detail::Working,
        };
        Notice { slot, kind, detail }
    });
    own.into_iter().chain((0..node.asks).map(move |_| Notice {
        slot,
        kind: NoticeKind::NeedsInput,
        detail: Detail::Approval,
    }))
}

impl ProjectModel {
    /// Every notice in the model, sorted (slot, then urgency).
    pub fn notices(&self) -> Vec<Notice> {
        let mut out: Vec<Notice> = self
            .plan_nodes()
            .flat_map(|(node, slot)| notices_of(node, slot))
            .collect();
        out.sort_unstable();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fold::{Carrier, aggregate, total};
    use kgraf_graph::snapshot::{Activity, ActivityKind, GraphSnapshot, SceneNode};

    #[test]
    fn every_state_maps_to_its_kind_and_waiting_states_are_not_notices() {
        assert_eq!(
            NoticeKind::of_state(WorkState::GateHolding),
            Some(NoticeKind::Failed)
        );
        assert_eq!(
            NoticeKind::of_state(WorkState::ScheduleDisabled),
            Some(NoticeKind::Failed)
        );
        assert_eq!(
            NoticeKind::of_state(WorkState::HeldForUser),
            Some(NoticeKind::NeedsInput)
        );
        assert_eq!(
            NoticeKind::of_state(WorkState::InFlight),
            Some(NoticeKind::Working)
        );
        for s in [
            WorkState::Done,
            WorkState::Blocked,
            WorkState::HeldByCap,
            WorkState::Colliding,
            WorkState::Waiting,
        ] {
            assert_eq!(NoticeKind::of_state(s), None, "{s:?}");
        }
        assert!(NoticeKind::Failed < NoticeKind::NeedsInput);
    }

    fn node(key: &str, state: WorkState, asks: usize) -> PlanNode {
        PlanNode {
            key: key.into(),
            state,
            label: key.into(),
            blocked_by: Vec::new(),
            asks,
            parent: None,
        }
    }

    /// One question on a task holding 30 files counted 32. It is
    /// one notice now, on the work node, whatever the leases.
    #[test]
    fn one_question_on_a_task_with_thirty_leases_counts_once() {
        let path = "src/a.rs";
        let nodes: Vec<SceneNode> = (0..30)
            .map(|i| SceneNode {
                id: format!("n{i}"),
                path: path.into(),
                kind: kgraf_graph::NodeKind::Function,
                qualified_name: format!("f{i}"),
                position: kgraf_graph::layout::position_for("src", &format!("n{i}")),
            })
            .collect();
        let activity = nodes
            .iter()
            .map(|n| Activity {
                agent: "run/c1@7".into(),
                task: "run/c1".into(),
                kind: ActivityKind::Holds,
                path: path.into(),
                node_id: Some(n.id.clone()),
            })
            .collect();
        let snap = GraphSnapshot {
            nodes,
            activity,
            ..GraphSnapshot::default()
        };
        let (mut m, mut scene) = ProjectModel::build(&snap).expect("build");
        m.set_plan(&mut scene, &[node("run/c1", WorkState::InFlight, 1)])
            .expect("plan");
        let n = m.notices();
        let work = m.task_slot("run/c1").expect("work");
        assert!(n.iter().all(|x| x.slot == work), "only on the work node");
        let badges = aggregate(&n, |s| Some(Carrier::Node(s)));
        assert_eq!(total(&badges).attention(), 1);
        assert_eq!(
            total(&badges).of(NoticeKind::Working),
            1,
            "data, not counted"
        );

        // A gate holding the work is one failure, not one per lease.
        m.set_plan(
            &mut scene,
            &[
                node("run/c1", WorkState::InFlight, 0),
                node("run/gate/g", WorkState::GateHolding, 0),
            ],
        )
        .expect("plan");
        let badges = aggregate(&m.notices(), |s| Some(Carrier::Node(s)));
        let t = total(&badges);
        assert_eq!((t.of(NoticeKind::Failed), t.attention()), (1, 1));
    }

    #[test]
    fn a_held_node_with_approvals_counts_the_hold_and_each_approval() {
        let got: Vec<NoticeKind> = notices_of(&node("k", WorkState::HeldForUser, 2), 5)
            .map(|n| n.kind)
            .collect();
        assert_eq!(got, vec![NoticeKind::NeedsInput; 3]);
        assert_eq!(notices_of(&node("k", WorkState::Done, 0), 5).count(), 0);
    }
}
