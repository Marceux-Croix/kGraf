//! Straight chains of a fresh plan become one contract, before interfaces are
//! derived (decision 0014, amendment of 2026-10-14; the rule is
//! [`crate::plan_chain`]).
//!
//! ```text
//!   plan accepted by the rules --> chains found? --no--> unchanged
//!        |yes
//!        v
//!   plan review on, or user order set? --yes--> kept as written, noticed
//!        |no
//!        v   per chain
//!   one route known before dispatch? --no--> kept, noticed
//!   tools offered by that route?     --no--> kept, noticed
//!   merged block over the plan target, and a split could fit one? --yes--> kept, noticed
//!        |yes
//!        v
//!   merged: `chain_collapsed` record and a run notice
//! ```
//!
//! The route is known before dispatch only when the roster has one route
//! (every routing policy), or when every step pins the same route. Under
//! rotation with several routes the cursor moves per dispatch, and under
//! fitness the pick follows measured figures: either may give the steps
//! different models, so a mixed-model arm keeps its split.
//!
//! A chain whose merged block is over one worker's plan target is kept whole as
//! the planner wrote it when a split could bring a step under the target (the
//! plan loop's own test, `Projection::split_can_fit`), not cut at the size
//! point: the planner chose that split, and the plan loop's splitting (decision
//! 0057) still applies to any contract over the target. When the worker prefix
//! and the projected growth alone are over the target, no step fits and no split
//! could make one fit, so the chain is merged: splitting would only add
//! interfaces and handshakes.

use super::*;
use crate::plan_chain::{self, Outcome, Verdict};

impl RunLoop {
    /// `specs` with each collapsible chain merged. Only a whole plan is
    /// collapsed: a split's children (`outer` not empty) exist because their
    /// parent was too big for one worker.
    pub(super) async fn collapse_chains(
        &mut self,
        specs: Vec<ContractSpec>,
        outer: &BTreeSet<String>,
    ) -> Result<Vec<ContractSpec>, OrchestratorError> {
        if !outer.is_empty() || !self.config.collapse_chains || specs.len() < 2 {
            return Ok(specs);
        }
        let chains = plan_chain::straight_chains(&specs);
        let Some(first) = chains.first() else {
            return Ok(specs);
        };
        if let Some(why) = self.plan_as_written() {
            let at = self.now();
            let head = specs[first[0]].id.clone();
            self.trace(TraceEvent::Notice {
                at_ms: at,
                contract: head,
                worker: String::new(),
                detail: format!(
                    "{} straight chain(s) kept as the planner wrote them: {why}",
                    chains.len()
                ),
            })
            .await?;
            return Ok(specs);
        }
        let (collapsed, outcomes) = plan_chain::collapse(specs.clone(), |links, merged| {
            self.chain_verdict(links, merged)
        });
        // A merged plan breaks a plan rule only if a rule this module does not
        // know of does; the planner's own plan then stands, and the refusal is
        // traced.
        let checked = validate_plan_with(
            &collapsed,
            &BTreeSet::new(),
            &self.roster(),
            &self.tools(),
            self.config.max_contracts_per_plan,
            self.bypass_git_files(),
        );
        if let Err(e) = checked {
            let at = self.now();
            self.trace(TraceEvent::Notice {
                at_ms: at,
                contract: specs[first[0]].id.clone(),
                worker: String::new(),
                detail: format!(
                    "straight chains kept as planned: the merged plan was refused ({e})"
                ),
            })
            .await?;
            return Ok(specs);
        }
        for outcome in outcomes {
            let at = self.now();
            match outcome {
                Outcome::Collapsed {
                    ids,
                    route,
                    projected_tokens,
                    target_tokens,
                } => {
                    self.intake_notice(kgraf_secrets::scrub_persist_owned(format!(
                        "contracts {} are a straight chain, each waiting on the one before; \
                         they were merged into `{}` on route `{route}` (projected {projected_tokens} \
                         tokens of {target_tokens})",
                        ids.join(" -> "),
                        ids[0]
                    )));
                    self.trace(TraceEvent::ChainCollapsed {
                        at_ms: at,
                        contract: ids[0].clone(),
                        merged: ids,
                        route,
                        projected_tokens,
                        target_tokens,
                    })
                    .await?;
                }
                Outcome::Kept { ids, reason } => {
                    self.trace(TraceEvent::Notice {
                        at_ms: at,
                        contract: ids[0].clone(),
                        worker: String::new(),
                        detail: kgraf_secrets::scrub_persist_owned(format!(
                            "straight chain {} kept as planned: {reason}",
                            ids.join(" -> ")
                        )),
                    })
                    .await?;
                }
            }
        }
        Ok(collapsed)
    }

    /// Why the plan stands as written, when a setting says it does: plan
    /// review (the user reads the planner's plan and may ask for its shape) or
    /// an order the user chose (0014).
    fn plan_as_written(&self) -> Option<&'static str> {
        if self.perms.control.plan_review() {
            Some("plan review is on")
        } else if !self.record.coordination.user_order.is_empty() {
            Some("the user chose an order")
        } else {
            None
        }
    }

    /// One chain's ruling: the route every step runs on must be known now,
    /// offer the merged tools, and carry the merged block.
    fn chain_verdict(&self, links: &[&ContractSpec], merged: &ContractSpec) -> Verdict {
        let roster = self.roster();
        let pins: BTreeSet<&str> = links.iter().filter_map(|l| l.route.as_deref()).collect();
        let pinned_by_all = links.iter().all(|l| l.route.is_some());
        let known = match roster.as_slice() {
            // One route: every step runs on it, pinned or not.
            [only] if pins.iter().all(|p| *p == only.as_str()) => Some(only.clone()),
            // Several routes: known only when every step pins the same one.
            _ if pinned_by_all && pins.len() == 1 => pins.iter().next().map(|p| (*p).to_owned()),
            _ => None,
        };
        let route = match known {
            Some(route) => route,
            None => {
                return Verdict::Keep(format!(
                    "its steps may run on different routes ({} routes, {} routing, {} distinct \
                     pin(s) among {} steps); one route is known before dispatch only when the \
                     roster has one or every step pins the same",
                    roster.len(),
                    self.config.routing,
                    pins.len(),
                    links.len()
                ));
            }
        };
        if !self.route_offers(&route, &merged.tools) {
            return Verdict::Keep(format!(
                "route `{route}` does not offer every tool the steps name together"
            ));
        }
        let Some(r) = self.route(&route) else {
            return Verdict::Keep(format!("route `{route}` is not configured"));
        };
        // The plan loop's sizing, without its traces: the named slices, else
        // the declared files' symbols.
        let from = merged.writes.first().map(|w| w.path.clone());
        let slice_tokens = match self.lookup_slices(&merged.slices, from) {
            Ok(found) if !found.nodes.is_empty() => found.tokens,
            Ok(_) => match self.write_file_slices(&merged.writes) {
                Ok((_, tokens, _)) => tokens,
                Err(e) => return Verdict::Keep(format!("its size could not be projected ({e})")),
            },
            Err(e) => return Verdict::Keep(format!("its size could not be projected ({e})")),
        };
        let base = self.base_tokens(r, &merged.instruction, slice_tokens);
        let projection = self
            .sizer
            .project(r.model(), base, self.config.step_budget, &r.context);
        // Over the target is a reason to keep the split only when a split can
        // bring a child under it (the plan loop's own test, `split_can_fit`).
        // When the worker prefix and the projected growth alone are over the
        // target, every step is over it too: splitting cannot make one worker
        // fit, and each split still costs an interface and a handshake.
        let fixed = self.base_tokens(r, "", 0);
        if !projection.fits() && projection.split_can_fit(fixed) {
            return Verdict::Keep(format!(
                "one worker cannot carry it: projected {} tokens exceed the plan target of {} \
                 ({:?}), and its steps are smaller",
                projection.tokens, projection.target_tokens, projection.source
            ));
        }
        Verdict::Collapse {
            route,
            projected_tokens: projection.tokens,
            target_tokens: projection.target_tokens,
        }
    }
}
