//! What waits for the user in the orchestrator box besides approval asks and
//! questions (decision 0094): permission requests raised by workers'
//! shortfalls, and the plan when plan review is on.
//!
//! The run loop owns the decisions; the desk only shows and carries answers.
//! [`ReviewDesk::answer_permission`] and [`ReviewDesk::answer_plan`] queue an
//! answer and wake the loop, which applies it on its own task (the record is
//! the loop's alone). Reading the desk never blocks on the loop, so the box
//! can draw it every frame.
//!
//! Requests are shown grouped: [`kgraf_tools::permissions::group_requests`]
//! puts the requests raised within
//! [`kgraf_tools::permissions::GROUP_WINDOW_MS`] of a group's first one into
//! that group, and the box answers one item or the whole group.

use crate::box_port::Wake;
use crate::shortfall::RequestScope;
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

/// One permission request waiting for the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionRequestView {
    pub id: u64,
    pub run: String,
    pub contract: String,
    pub worker: String,
    pub scope: RequestScope,
    /// The program or path asked for.
    pub subject: String,
    pub why: String,
    pub question: String,
    pub raised_at_ms: i64,
}

impl kgraf_tools::permissions::Raised for PermissionRequestView {
    fn raised_at_ms(&self) -> i64 {
        self.raised_at_ms
    }
}

/// One contract of a plan shown for review.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanItemView {
    pub contract: String,
    pub instruction: String,
    pub acceptance: String,
    /// Contracts that must be verified first.
    pub depends_on: Vec<String>,
    /// What it writes: repository paths.
    pub writes: Vec<String>,
    /// What it reads: graph slice anchors.
    pub slices: Vec<String>,
    pub route: Option<String>,
}

/// The plan waiting for the user's yes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanView {
    pub run: String,
    /// 1 for the first plan, one more after each change.
    pub revision: u32,
    pub items: Vec<PlanItemView>,
}

impl PlanView {
    /// Every path the plan writes, for the graph's highlight.
    pub fn touched_paths(&self) -> Vec<String> {
        let mut paths: Vec<String> = self
            .items
            .iter()
            .flat_map(|i| i.writes.iter().cloned())
            .collect();
        paths.sort();
        paths.dedup();
        paths
    }
}

/// The user's answer to a plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlanAnswer {
    /// Dispatch it.
    Approve,
    /// Dispatch nothing; the run ends blocked.
    Reject { reason: String },
    /// Plan again with this note from the user.
    Change { note: String },
}

/// Why an answer was not taken.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ReviewError {
    #[error("no permission request {0} is waiting: it was answered already, or its run ended")]
    UnknownRequest(u64),
    #[error("no plan is waiting for review")]
    NoPlan,
    #[error("a change needs a note saying what to change")]
    EmptyChange,
}

#[derive(Default)]
struct State {
    requests: BTreeMap<u64, PermissionRequestView>,
    answers: VecDeque<(u64, bool)>,
    plan: Option<PlanView>,
    plan_answer: Option<PlanAnswer>,
}

/// Permission requests and the plan, waiting for the user.
#[derive(Default)]
pub struct ReviewDesk {
    state: Mutex<State>,
    notify: tokio::sync::Notify,
    wake: Mutex<Option<Wake>>,
}

impl std::fmt::Debug for ReviewDesk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = self.state();
        f.debug_struct("ReviewDesk")
            .field("requests", &s.requests.len())
            .field("plan", &s.plan.is_some())
            .finish()
    }
}

impl ReviewDesk {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Run `wake` whenever what the desk shows changes (the box repaints).
    pub fn set_wake(&self, wake: Wake) {
        *self.wake.lock().unwrap_or_else(|p| p.into_inner()) = Some(wake);
    }

    fn woke(&self) {
        let wake = self.wake.lock().unwrap_or_else(|p| p.into_inner()).clone();
        if let Some(w) = wake {
            w();
        }
    }

    /// Requests waiting for the user, oldest first.
    pub fn pending_permissions(&self) -> Vec<PermissionRequestView> {
        let mut v: Vec<_> = self.state().requests.values().cloned().collect();
        v.sort_by_key(|r| (r.raised_at_ms, r.id));
        v
    }

    /// Grant or deny one request. The loop applies it.
    pub fn answer_permission(&self, id: u64, grant: bool) -> Result<(), ReviewError> {
        {
            let mut s = self.state();
            if s.requests.remove(&id).is_none() {
                return Err(ReviewError::UnknownRequest(id));
            }
            s.answers.push_back((id, grant));
        }
        self.notify.notify_one();
        self.woke();
        Ok(())
    }

    /// The plan waiting for the user's yes, if any.
    pub fn pending_plan(&self) -> Option<PlanView> {
        self.state().plan.clone()
    }

    /// Answer the plan. The loop applies it.
    pub fn answer_plan(&self, answer: PlanAnswer) -> Result<(), ReviewError> {
        if let PlanAnswer::Change { note } = &answer
            && note.trim().is_empty()
        {
            return Err(ReviewError::EmptyChange);
        }
        {
            let mut s = self.state();
            if s.plan.take().is_none() {
                return Err(ReviewError::NoPlan);
            }
            s.plan_answer = Some(answer);
        }
        self.notify.notify_one();
        self.woke();
        Ok(())
    }

    pub(crate) fn show_request(&self, view: PermissionRequestView) {
        self.state().requests.insert(view.id, view);
        self.woke();
    }

    pub(crate) fn drop_request(&self, id: u64) {
        let removed = self.state().requests.remove(&id).is_some();
        if removed {
            self.woke();
        }
    }

    pub(crate) fn take_answers(&self) -> Vec<(u64, bool)> {
        self.state().answers.drain(..).collect()
    }

    pub(crate) fn show_plan(&self, plan: PlanView) {
        {
            let mut s = self.state();
            s.plan = Some(plan);
            s.plan_answer = None;
        }
        self.woke();
    }

    pub(crate) fn clear_plan(&self) {
        let had = self.state().plan.take().is_some();
        if had {
            self.woke();
        }
    }

    pub(crate) fn take_plan_answer(&self) -> Option<PlanAnswer> {
        self.state().plan_answer.take()
    }

    /// Wakes when an answer arrives. An answer given before anyone waits is
    /// not lost: `Notify` keeps one permit.
    pub(crate) fn answered(
        self: &Arc<Self>,
    ) -> impl std::future::Future<Output = ()> + Send + 'static {
        let me = self.clone();
        async move { me.notify.notified().await }
    }

    /// Clear everything: the run ended.
    pub(crate) fn close(&self) {
        {
            let mut s = self.state();
            s.requests.clear();
            s.answers.clear();
            s.plan = None;
            s.plan_answer = None;
        }
        self.woke();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(id: u64, at: i64) -> PermissionRequestView {
        PermissionRequestView {
            id,
            run: "r".into(),
            contract: format!("c{id}"),
            worker: format!("w{id}"),
            scope: RequestScope::Command,
            subject: "python3".into(),
            why: "x".into(),
            question: "grant?".into(),
            raised_at_ms: at,
        }
    }

    #[tokio::test]
    async fn an_answer_leaves_the_desk_reaches_the_loop_and_cannot_be_given_twice() {
        let desk = Arc::new(ReviewDesk::default());
        desk.show_request(req(1, 0));
        desk.show_request(req(2, 10));
        let waiting = desk.answered();
        desk.answer_permission(2, true).expect("answer");
        tokio::time::timeout(std::time::Duration::from_secs(1), waiting)
            .await
            .expect("woken");
        assert_eq!(desk.take_answers(), vec![(2, true)]);
        assert_eq!(desk.pending_permissions(), vec![req(1, 0)]);
        assert_eq!(
            desk.answer_permission(2, false),
            Err(ReviewError::UnknownRequest(2))
        );
    }

    #[test]
    fn requests_close_in_time_group_together() {
        let groups =
            kgraf_tools::permissions::group_requests(&[req(1, 0), req(2, 500), req(3, 5_000)]);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].len(), 2);
    }

    #[test]
    fn a_plan_answer_needs_a_plan_and_a_change_needs_a_note() {
        let desk = ReviewDesk::default();
        assert_eq!(
            desk.answer_plan(PlanAnswer::Approve),
            Err(ReviewError::NoPlan)
        );
        desk.show_plan(PlanView {
            run: "r".into(),
            revision: 1,
            items: vec![],
        });
        assert_eq!(
            desk.answer_plan(PlanAnswer::Change { note: " ".into() }),
            Err(ReviewError::EmptyChange)
        );
        assert!(
            desk.pending_plan().is_some(),
            "a refused answer keeps the plan"
        );
        desk.answer_plan(PlanAnswer::Approve).expect("approve");
        assert!(desk.pending_plan().is_none());
        assert_eq!(desk.take_plan_answer(), Some(PlanAnswer::Approve));
    }
}
