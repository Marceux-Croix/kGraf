//! A project view's fold state (decision 0068): the automatic cut, the user's
//! expansions, and the glue between the model's hierarchy, the galaxy and a click.
//!
//! Everything is kept by [`ProjectModel::fold_key`] (a graph id or a proxy key), not by
//! slot, so it survives a reload that renumbers every slot: the user's expansions, and
//! the last automatic cut, which the next cut starts from (hysteresis, see
//! [`crate::fold::auto_expand_from`]). They live as long as the project view, as the
//! camera does; nothing in the view state is written to disk, so neither is this.
//!
//! A refresh with nothing new (the same hierarchy generation and the same user choices)
//! does nothing: a change batch that only moves or restyles code costs no refold.

use std::collections::{HashMap, HashSet};

use crate::fold::{self, Badge, Carrier, Fold, Toggle, VISIBLE_BUDGET};
use crate::galaxy::Galaxy;
use crate::project::ProjectModel;
use crate::view::Click;

/// The fold of one project view.
#[derive(Debug, Clone)]
pub struct Chunks {
    budget: usize,
    /// The user's choices by fold key: expanded (`true`) or folded (`false`).
    overrides: HashMap<String, bool>,
    overrides_version: u64,
    /// The last automatic cut, by fold key of each expanded node.
    last_cut: HashSet<String>,
    /// What the last refresh was computed from: hierarchy generation, overrides version.
    applied: Option<(u64, u64)>,
    fold: Fold,
}

impl Default for Chunks {
    fn default() -> Self {
        Self::new(VISIBLE_BUDGET)
    }
}

impl Chunks {
    pub fn new(budget: usize) -> Self {
        Self {
            budget,
            overrides: HashMap::new(),
            overrides_version: 0,
            last_cut: HashSet::new(),
            applied: None,
            fold: Fold::default(),
        }
    }

    pub fn budget(&self) -> usize {
        self.budget
    }

    /// The fold as last applied.
    pub fn fold(&self) -> &Fold {
        &self.fold
    }

    /// Drawn members over the budget, when the user's expansions (or roots beyond the
    /// fan-out, which the hierarchy prevents) drew more than it allows: `(drawn,
    /// budget)`. The view says so; it never hides it.
    pub fn over_budget(&self) -> Option<(usize, usize)> {
        let drawn = self.fold.visible_count();
        (drawn > self.budget).then_some((drawn, self.budget))
    }

    /// Marks the next refresh as a new start: a reload built a new model, whose
    /// hierarchy generation says nothing about the old one. The last cut is kept (by
    /// key), so the view does not reshuffle on a reload.
    pub fn forget_cut(&mut self) {
        self.applied = None;
    }

    /// Recomputes the fold from the model and applies it to `galaxy`, unless nothing it
    /// reads changed. `animate` is off for a load.
    pub fn refresh(&mut self, model: &ProjectModel, galaxy: &mut Galaxy, animate: bool) {
        let h = model.hierarchy();
        let now = (h.generation(), self.overrides_version);
        if self.applied == Some(now) {
            return;
        }
        let scene = galaxy.scene();
        let alive = |s: u32| scene.is_alive(s);
        let mut prev = vec![false; h.len()];
        for key in &self.last_cut {
            if let Some(s) = model.slot_of_fold_key(key)
                && let Some(p) = prev.get_mut(s as usize)
            {
                *p = true;
            }
        }
        let auto = fold::auto_expand_from(h, alive, self.budget, &prev);
        self.last_cut = auto
            .iter()
            .enumerate()
            .filter(|(_, e)| **e)
            .filter_map(|(s, _)| model.fold_key(s as u32))
            .collect();
        let overrides: HashMap<u32, bool> = self
            .overrides
            .iter()
            .filter_map(|(k, v)| model.slot_of_fold_key(k).map(|s| (s, *v)))
            .collect();
        self.fold = fold::resolve(h, alive, &auto, &overrides);
        galaxy.set_fold(&self.fold, h, animate);
        self.applied = Some(now);
    }

    /// Expands or folds one node by the user's hand, then refreshes.
    pub fn toggle(&mut self, model: &ProjectModel, galaxy: &mut Galaxy, toggle: Toggle) {
        let (slot, open) = match toggle {
            Toggle::Expand(s) => (s, true),
            Toggle::Collapse(s) => (s, false),
        };
        if let Some(key) = model.fold_key(slot) {
            self.overrides.insert(key, open);
            self.overrides_version += 1;
            self.refresh(model, galaxy, true);
        }
    }

    /// Applies the click rule ([`fold::on_click`]). Returns what it did.
    pub fn click(
        &mut self,
        model: &ProjectModel,
        galaxy: &mut Galaxy,
        click: Click,
    ) -> Option<Toggle> {
        let t = fold::on_click(&self.fold, click.hit, click.was_selected)?;
        self.toggle(model, galaxy, t);
        Some(t)
    }

    /// What carries a notice on `slot` now: the slot when drawn, else its drawn
    /// ancestor when folded, else the CPU-budget group standing for that (style guide
    /// STG-DRAW-8: the count stacks on the nearest visible ancestor). `None` only for a
    /// dead slot.
    pub fn carrier(&self, galaxy: &Galaxy, slot: u32) -> Option<Carrier> {
        if !galaxy.scene().is_alive(slot) {
            return None;
        }
        let s = if galaxy.is_drawn(slot) {
            slot
        } else {
            self.fold.carrier(slot).unwrap_or(slot)
        };
        if galaxy.is_drawn(s) {
            return Some(Carrier::Node(s));
        }
        Some(match galaxy.group_of(s) {
            Some(g) => Carrier::Group(g),
            // Not drawn for another reason (it is vanishing): counted, not drawn.
            None => Carrier::Node(s),
        })
    }

    /// The model's notices stacked on what `galaxy` draws now.
    pub fn badges(&self, model: &ProjectModel, galaxy: &Galaxy) -> Vec<Badge> {
        fold::aggregate(&model.notices(), |s| self.carrier(galaxy, s))
    }
}
