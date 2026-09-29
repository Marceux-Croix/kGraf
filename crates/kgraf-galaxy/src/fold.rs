//! Chunking (decision 0068): children fold into their parents so a large graph stays
//! readable, a click expands a folded node, and notices on hidden nodes stack on the
//! nearest drawn ancestor. Everything here is pure: no scene, no GPU, no egui.
//!
//! **The hierarchy** ([`Hierarchy`]) is built by `project.rs`: a chunk under its
//! `contains` parent, a file under its directory, a directory under its parent
//! directory (single-child directory chains merged), and any parent with more than
//! [`MAX_FANOUT`] children splits them into group proxies ([`plan_groups`]). So every
//! node, the invisible root included, has at most [`MAX_FANOUT`] children. Work nodes
//! (plan nodes) join as manual members under their plan parent (`plan.rs`): the cut
//! never expands them, so they stay folded until the user expands them. Agents are not
//! members: they never fold.
//!
//! **The automatic cut** ([`auto_expand_from`]). Every root is drawn. Parents are
//! expanded in ascending order of (depth, live child count, rank, slot) while the drawn
//! total stays at or under the budget: the deepest levels and the largest parents fold
//! first. **Hysteresis:** the previous cut is kept while it fits. When it no longer
//! fits, expanded parents with no expanded child fold, deepest and smallest first,
//! until it does; then the cut grows again where room is left. So one change moves the
//! view by about the size of that change, not by the size of a level.
//!
//! **User overrides** ([`resolve`]) sit on top of the cut and never re-run the budget,
//! so expanding one node never folds another. They may exceed the budget; the view then
//! says so ("over budget"), it never hides it.
//!
//! **Notices** ([`aggregate`]) do not unfold anything. A notice on a hidden node counts
//! on its drawn ancestor. Only failures and needs-input count, and
//! a failure counts as needs input: the count pill is yellow either way.
//!
//! Written for kGraf. Searched first (2026-09-27): crates.io for tree collapse, tree
//! folding, tree aggregation, hierarchical clustering, graph coarsening and degree of
//! interest. The clustering crates (`kodama`, `linfa-hierarchical`, `metis-rs`) build a
//! hierarchy from features, and this one is given; `hylic` is a general tree fold
//! framework, more than a cut needs. The reference galaxy (codebase-memory-mcp,
//! evidence 2026-09-24) never aggregates.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::ops::Range;

/// Most hierarchy nodes drawn by the automatic cut. Chosen, not measured for legibility:
/// this repository has about 530 files and 90 directories in 13,000 nodes, so 1,000
/// draws every directory and file plus the contents of the smallest files. To be tuned
/// with the owner. User expansions may exceed it, and the view says so.
pub const VISIBLE_BUDGET: usize = 1_000;

/// Most children any node has, the invisible root included. More are split into group
/// proxies ("+N more"), so expanding one node draws at most this many more nodes and the
/// roots alone never exceed the budget. Chosen: a file of 64 items is still one level.
pub const MAX_FANOUT: usize = 64;

/// Largest size multiplier of a folded proxy. A folded directory or group proxy grows
/// with the square root of what it hides, up to this. Code dots never grow: their radius
/// is their connection count's (style guide STG-DRAW-2).
pub const FOLD_SCALE_MAX: f32 = 3.0;

const NONE: u32 = u32::MAX;

/// Parent and children per scene slot. Slots that were never [`Hierarchy::set`] are
/// not members: roots that no fold touches (agents, work items, free slots).
#[derive(Debug, Clone, Default)]
pub struct Hierarchy {
    parent: Vec<u32>,
    children: Vec<Vec<u32>>,
    rank: Vec<u64>,
    member: Vec<bool>,
    /// Whether the automatic cut may expand it. Work nodes may not: they stay folded
    /// unless the user expands them (decision 0068).
    auto: Vec<bool>,
    /// Bumped on every change, so a cached cut knows when to recompute.
    generation: u64,
}

impl Hierarchy {
    fn grow(&mut self, slot: u32) {
        let n = slot as usize + 1;
        if self.parent.len() < n {
            self.parent.resize(n, NONE);
            self.children.resize_with(n, Vec::new);
            self.rank.resize(n, 0);
            self.member.resize(n, false);
            self.auto.resize(n, true);
        }
    }

    /// [`Self::set`] for a node the automatic cut never expands: it stays folded
    /// unless the user expands it (work nodes, decision 0068).
    pub fn set_manual(&mut self, slot: u32, parent: Option<u32>, rank: u64) -> bool {
        if !self.set(slot, parent, rank) {
            return false;
        }
        let i = slot as usize;
        if self.auto[i] {
            self.auto[i] = false;
            self.generation += 1;
        }
        true
    }

    /// Whether the automatic cut may expand `slot`.
    pub fn is_auto(&self, slot: u32) -> bool {
        self.auto.get(slot as usize).copied().unwrap_or(true)
    }

    /// Makes `slot` a member under `parent` (a root with `None`), with `rank` as its
    /// tie-break. Refused (returns `false`, nothing changed) when `parent` is `slot`
    /// or one of its descendants: a cycle would hide both forever.
    ///
    /// The cycle check is O(1) when `slot` has no children (a node placed before its
    /// children, which is how the model builds), and a walk up from `parent` otherwise.
    pub fn set(&mut self, slot: u32, parent: Option<u32>, rank: u64) -> bool {
        if let Some(p) = parent {
            if p == slot {
                return false;
            }
            if !self.children(slot).is_empty() {
                let mut at = Some(p);
                while let Some(a) = at {
                    if a == slot {
                        return false;
                    }
                    at = self.parent(a);
                }
            }
            self.grow(p);
        }
        self.grow(slot);
        let i = slot as usize;
        let new_parent = parent.unwrap_or(NONE);
        if self.member[i] && self.parent[i] == new_parent && self.rank[i] == rank {
            return true;
        }
        self.detach(slot);
        self.member[i] = true;
        self.rank[i] = rank;
        self.parent[i] = new_parent;
        if let Some(p) = parent {
            self.children[p as usize].push(slot);
        }
        self.generation += 1;
        true
    }

    fn detach(&mut self, slot: u32) {
        let old = self.parent[slot as usize];
        if old != NONE {
            self.children[old as usize].retain(|&c| c != slot);
        }
    }

    /// Removes `slot` from the hierarchy. Returns its former children, now roots,
    /// for the caller to place again.
    pub fn remove(&mut self, slot: u32) -> Vec<u32> {
        let i = slot as usize;
        if i >= self.parent.len() || !self.member[i] {
            return Vec::new();
        }
        self.detach(slot);
        let orphans = std::mem::take(&mut self.children[i]);
        for &c in &orphans {
            self.parent[c as usize] = NONE;
        }
        self.parent[i] = NONE;
        self.member[i] = false;
        self.auto[i] = true;
        self.generation += 1;
        orphans
    }

    pub fn is_member(&self, slot: u32) -> bool {
        self.member.get(slot as usize).copied().unwrap_or(false)
    }

    pub fn parent(&self, slot: u32) -> Option<u32> {
        self.parent
            .get(slot as usize)
            .copied()
            .filter(|&p| p != NONE)
    }

    pub fn children(&self, slot: u32) -> &[u32] {
        self.children.get(slot as usize).map_or(&[][..], |c| c)
    }

    pub fn rank(&self, slot: u32) -> u64 {
        self.rank.get(slot as usize).copied().unwrap_or(0)
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Slots covered (members or not).
    pub fn len(&self) -> usize {
        self.parent.len()
    }

    pub fn is_empty(&self) -> bool {
        self.parent.is_empty()
    }

    /// Live member roots, in slot order.
    pub fn roots<'a>(&'a self, alive: &'a impl Fn(u32) -> bool) -> impl Iterator<Item = u32> + 'a {
        (0..self.len() as u32).filter(move |&s| {
            self.member[s as usize] && self.parent[s as usize] == NONE && alive(s)
        })
    }
}

/// The automatic cut with no history: [`auto_expand_from`] with nothing expanded before.
pub fn auto_expand(h: &Hierarchy, alive: impl Fn(u32) -> bool, budget: usize) -> Vec<bool> {
    auto_expand_from(h, alive, budget, &[])
}

/// The automatic cut: which members are expanded so that at most `budget` members are
/// drawn (unless the roots alone exceed it). Indexed by slot, `h.len()` long.
///
/// `prev` is the last cut (indexed by slot; shorter is fine). It is kept while it fits;
/// see the module note for what happens when it does not.
pub fn auto_expand_from(
    h: &Hierarchy,
    alive: impl Fn(u32) -> bool,
    budget: usize,
    prev: &[bool],
) -> Vec<bool> {
    let n = h.len();
    let live_children = |s: u32| h.children(s).iter().filter(|&&c| alive(c)).count();
    let mut expanded = vec![false; n];
    // Walk the previous cut from the roots: what it draws, and at what depth.
    let mut depth = vec![0u32; n];
    let mut used = 0usize;
    let mut stack: Vec<u32> = h.roots(&alive).collect();
    // Expanded parents in the order they were reached (for the fold phase).
    let mut open: Vec<u32> = Vec::new();
    while let Some(s) = stack.pop() {
        used += 1;
        let i = s as usize;
        if prev.get(i).copied().unwrap_or(false) && h.is_auto(s) && live_children(s) > 0 {
            expanded[i] = true;
            open.push(s);
            for &c in h.children(s) {
                if alive(c) {
                    depth[c as usize] = depth[i] + 1;
                    stack.push(c);
                }
            }
        }
    }

    // Fold phase: while over, fold an expanded parent none of whose children is
    // expanded, deepest first, then fewest children (the least change), then the
    // reverse of the grow order (rank, slot).
    if used > budget {
        let has_open_child =
            |s: u32, ex: &[bool]| h.children(s).iter().any(|&c| alive(c) && ex[c as usize]);
        let key = |s: u32| (depth[s as usize], Reverse(live_children(s)), h.rank(s), s);
        let mut heap: BinaryHeap<_> = open
            .iter()
            .copied()
            .filter(|&s| !has_open_child(s, &expanded))
            .map(key)
            .collect();
        while used > budget {
            let Some((_, _, _, s)) = heap.pop() else {
                break;
            };
            let i = s as usize;
            if !expanded[i] || has_open_child(s, &expanded) {
                continue;
            }
            expanded[i] = false;
            used -= live_children(s);
            if let Some(p) = h.parent(s)
                && !has_open_child(p, &expanded)
            {
                heap.push(key(p));
            }
        }
    }

    // Grow phase: ascending (depth, child count, rank, slot) over the drawn, folded
    // parents. A parent that does not fit is skipped; every later one at its depth has
    // at least as many children, and deeper ones may still fit.
    let mut heap = BinaryHeap::new();
    let mut stack: Vec<u32> = h.roots(&alive).collect();
    while let Some(s) = stack.pop() {
        let i = s as usize;
        if expanded[i] {
            for &c in h.children(s) {
                if alive(c) {
                    depth[c as usize] = depth[i] + 1;
                    stack.push(c);
                }
            }
        } else if h.is_auto(s) {
            let c = live_children(s);
            if c > 0 {
                heap.push(Reverse((depth[i], c, h.rank(s), s)));
            }
        }
    }
    while let Some(Reverse((d, c, _, s))) = heap.pop() {
        if used + c > budget {
            continue;
        }
        used += c;
        expanded[s as usize] = true;
        for &k in h.children(s) {
            if !alive(k) {
                continue;
            }
            depth[k as usize] = d + 1;
            let kc = live_children(k);
            if kc > 0 && h.is_auto(k) {
                heap.push(Reverse((d + 1, kc, h.rank(k), k)));
            }
        }
    }
    expanded
}

/// How a parent's sorted children are split into groups when there are more than
/// `fanout`. Each group covers a contiguous range of the sorted children; `parent` is
/// the index of the enclosing group, or `None` for a top group. Groups come parents
/// first. At most `fanout` groups sit at any level, and a group holds at most `fanout`
/// children (groups or items), so the tree is `log_fanout(n)` deep.
///
/// The number of groups at a level is a power of two, so a group's range moves by one
/// item per insert and the set of groups changes only when the count crosses a power
/// of two (as the layout's cluster discs do, 0058).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupPlan {
    /// Position among its siblings at each level from the top, the group's identity.
    pub path: Vec<u16>,
    pub range: Range<usize>,
    pub parent: Option<usize>,
    /// Whether its children are items (not further groups).
    pub leaf: bool,
}

/// Groups for `n` sorted children under a fan-out of `fanout` (at least 2). Empty when
/// `n <= fanout`.
pub fn plan_groups(n: usize, fanout: usize) -> Vec<GroupPlan> {
    let fanout = fanout.max(2);
    let mut out = Vec::new();
    if n <= fanout {
        return out;
    }
    fn split(
        range: Range<usize>,
        fanout: usize,
        path: Vec<u16>,
        parent: Option<usize>,
        out: &mut Vec<GroupPlan>,
    ) {
        let len = range.len();
        // The largest power of two not above `fanout`: at most `fanout` groups.
        let cap = 1usize << (usize::BITS - 1 - fanout.leading_zeros());
        let k = len.div_ceil(fanout).next_power_of_two().clamp(2, cap);
        for i in 0..k {
            let a = range.start + i * len / k;
            let b = range.start + (i + 1) * len / k;
            let mut p = path.clone();
            p.push(i as u16);
            let at = out.len();
            let leaf = b - a <= fanout;
            out.push(GroupPlan {
                path: p.clone(),
                range: a..b,
                parent,
                leaf,
            });
            if !leaf {
                split(a..b, fanout, p, Some(at), out);
            }
        }
    }
    split(0..n, fanout, Vec::new(), None, &mut out);
    out
}

/// What is drawn after the automatic cut and the user's overrides.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Fold {
    /// Members drawn. Non-members are not covered here (always drawn).
    visible: Vec<bool>,
    /// Members expanded (children drawn), effective after overrides.
    expanded: Vec<bool>,
    /// For a drawn, folded member: live descendants it hides.
    hidden_below: Vec<u32>,
    /// For a hidden member: the drawn ancestor it is folded into.
    carrier: Vec<u32>,
    visible_count: usize,
    hidden_count: usize,
}

impl Fold {
    /// Whether `slot` is a member the fold hides.
    pub fn is_hidden(&self, slot: u32) -> bool {
        self.carrier.get(slot as usize).is_some_and(|&c| c != NONE)
    }

    /// Whether `slot` is a drawn member.
    pub fn is_visible(&self, slot: u32) -> bool {
        self.visible.get(slot as usize).copied().unwrap_or(false)
    }

    /// Whether `slot` is a drawn member with its children drawn.
    pub fn is_expanded(&self, slot: u32) -> bool {
        let i = slot as usize;
        self.visible.get(i).copied().unwrap_or(false) && self.expanded[i]
    }

    /// Live descendants a drawn, folded member hides (0 otherwise).
    pub fn hidden_below(&self, slot: u32) -> u32 {
        self.hidden_below.get(slot as usize).copied().unwrap_or(0)
    }

    /// The drawn ancestor a hidden member is folded into.
    pub fn carrier(&self, slot: u32) -> Option<u32> {
        self.carrier
            .get(slot as usize)
            .copied()
            .filter(|&c| c != NONE)
    }

    /// Members drawn.
    pub fn visible_count(&self) -> usize {
        self.visible_count
    }

    /// Members hidden.
    pub fn hidden_count(&self) -> usize {
        self.hidden_count
    }

    /// Slots covered.
    pub fn len(&self) -> usize {
        self.carrier.len()
    }

    pub fn is_empty(&self) -> bool {
        self.carrier.is_empty()
    }

    /// Size multiplier for a folded proxy: grows with the square root of what it hides.
    pub fn scale(&self, slot: u32) -> f32 {
        let hidden = self.hidden_below(slot);
        if hidden == 0 {
            1.0
        } else {
            (1.0 + hidden as f32).sqrt().min(FOLD_SCALE_MAX)
        }
    }
}

/// Applies `overrides` (slot to expanded or not) on top of `auto` and walks the
/// hierarchy from its roots. O(members).
pub fn resolve(
    h: &Hierarchy,
    alive: impl Fn(u32) -> bool,
    auto: &[bool],
    overrides: &HashMap<u32, bool>,
) -> Fold {
    let n = h.len();
    let mut fold = Fold {
        visible: vec![false; n],
        expanded: vec![false; n],
        hidden_below: vec![0; n],
        carrier: vec![NONE; n],
        visible_count: 0,
        hidden_count: 0,
    };
    let expanded = |s: u32| {
        overrides
            .get(&s)
            .copied()
            .unwrap_or_else(|| auto.get(s as usize).copied().unwrap_or(false))
    };
    // Depth-first over drawn members; a folded one hands its subtree to `hide`.
    let mut stack: Vec<u32> = h.roots(&alive).collect();
    let mut hide: Vec<u32> = Vec::new();
    while let Some(s) = stack.pop() {
        let i = s as usize;
        fold.visible[i] = true;
        fold.visible_count += 1;
        let open = expanded(s);
        fold.expanded[i] = open;
        let live = h.children(s).iter().copied().filter(|&c| alive(c));
        if open {
            stack.extend(live);
        } else {
            // Everything below `s` is hidden and carried by `s`.
            hide.clear();
            hide.extend(live);
            let mut count = 0u32;
            while let Some(d) = hide.pop() {
                fold.carrier[d as usize] = s;
                count += 1;
                hide.extend(h.children(d).iter().copied().filter(|&c| alive(c)));
            }
            fold.hidden_below[i] = count;
            fold.hidden_count += count as usize;
        }
    }
    fold
}

/// What a click on the galaxy does to the fold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Toggle {
    Expand(u32),
    Collapse(u32),
}

/// The click rule: a folded node expands (and is selected, as any click selects);
/// a click on the node that was already selected folds it again if it is expanded;
/// any other click only selects. The fold handles ("+N", "-") toggle directly.
pub fn on_click(fold: &Fold, hit: Option<u32>, was_selected: bool) -> Option<Toggle> {
    let n = hit?;
    if fold.hidden_below(n) > 0 {
        return Some(Toggle::Expand(n));
    }
    if was_selected && fold.is_expanded(n) {
        return Some(Toggle::Collapse(n));
    }
    None
}

/// A notice's kind, most urgent first (the derived `Ord` is the urgency order).
///
/// Only [`NoticeKind::Failed`] and [`NoticeKind::NeedsInput`] are counted and drawn.
/// A failure needs the user, so it counts in the yellow pill like
/// any needs-input item; orange marks only the failed item itself (STG-DRAW-11). Working and new
/// results are kept as data so a marker without a count can be added later; the agent circles and
/// leases already show activity, and new results belong to the project dot and the live activity
/// card.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NoticeKind {
    /// A gate that failed or holds the work below, or a schedule that stopped itself.
    Failed,
    /// Held for the user, or a question or approval waiting.
    NeedsInput,
    /// Work in flight (data only; never counted).
    Working,
    /// New results (data only; never counted).
    NewResults,
}

impl NoticeKind {
    pub const ALL: [NoticeKind; 4] = [
        NoticeKind::Failed,
        NoticeKind::NeedsInput,
        NoticeKind::Working,
        NoticeKind::NewResults,
    ];

    /// Whether the user must act: counted on the pills.
    pub fn needs_attention(self) -> bool {
        matches!(self, NoticeKind::Failed | NoticeKind::NeedsInput)
    }

    pub fn word(self) -> &'static str {
        match self {
            NoticeKind::Failed => "failed",
            NoticeKind::NeedsInput => "needs input",
            NoticeKind::Working => "working",
            NoticeKind::NewResults => "new results",
        }
    }
}

/// What a notice is about, for the words on a tooltip ("1 failed gate, 2 approvals").
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Detail {
    FailedGate,
    StoppedSchedule,
    HeldForYou,
    Approval,
    Working,
}

impl Detail {
    pub const ALL: [Detail; 5] = [
        Detail::FailedGate,
        Detail::StoppedSchedule,
        Detail::HeldForYou,
        Detail::Approval,
        Detail::Working,
    ];

    /// The words for `n` of them.
    pub fn words(self, n: u32) -> String {
        let (one, many) = match self {
            Detail::FailedGate => ("failed gate", "failed gates"),
            Detail::StoppedSchedule => ("stopped schedule", "stopped schedules"),
            Detail::HeldForYou => ("held for you", "held for you"),
            Detail::Approval => ("approval", "approvals"),
            Detail::Working => ("working", "working"),
        };
        format!("{n} {}", if n == 1 { one } else { many })
    }
}

/// One notice on one node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Notice {
    pub slot: u32,
    pub kind: NoticeKind,
    pub detail: Detail,
}

/// Counts per kind, in [`NoticeKind::ALL`] order, and per detail, in [`Detail::ALL`]
/// order.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tally {
    pub counts: [u32; 4],
    pub details: [u32; 5],
}

impl Tally {
    pub fn of(&self, kind: NoticeKind) -> u32 {
        self.counts[kind as usize]
    }

    /// What is counted: failures plus needs-input.
    pub fn attention(&self) -> u32 {
        self.of(NoticeKind::Failed) + self.of(NoticeKind::NeedsInput)
    }

    /// The most urgent counted kind, if any.
    pub fn most_urgent(&self) -> Option<NoticeKind> {
        [NoticeKind::Failed, NoticeKind::NeedsInput]
            .into_iter()
            .find(|&k| self.of(k) > 0)
    }

    /// The counted notices in words, by what they are, most urgent first: "1 failed
    /// gate, 2 approvals" (the owner's example).
    pub fn words(&self) -> String {
        Detail::ALL
            .into_iter()
            .zip(self.details)
            .filter(|(d, n)| *n > 0 && *d != Detail::Working)
            .map(|(d, n)| d.words(n))
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn add(&mut self, n: &Notice) {
        self.counts[n.kind as usize] += 1;
        self.details[n.detail as usize] += 1;
    }
}

/// Where a badge is drawn: on a drawn node, or on a CPU-budget group sprite that stands
/// for a node the render cap did not draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Carrier {
    Node(u32),
    Group(u32),
}

/// The notices one carrier holds: its own and those of every node it stands for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Badge {
    pub at: Carrier,
    pub tally: Tally,
    /// The notices, for the tooltip and the accessible name.
    pub sources: Vec<Notice>,
}

/// Stacks each notice on what carries it (`carrier(slot)`: the slot itself when drawn,
/// else its drawn ancestor or its group, else `None`, which only a dead slot gets). One
/// badge per carrier, in carrier order.
pub fn aggregate(notices: &[Notice], carrier: impl Fn(u32) -> Option<Carrier>) -> Vec<Badge> {
    let mut by: HashMap<Carrier, Badge> = HashMap::new();
    for n in notices {
        if let Some(c) = carrier(n.slot) {
            let b = by.entry(c).or_insert_with(|| Badge {
                at: c,
                tally: Tally::default(),
                sources: Vec::new(),
            });
            b.tally.add(n);
            b.sources.push(*n);
        }
    }
    let mut out: Vec<Badge> = by.into_values().collect();
    out.sort_unstable_by_key(|b| b.at);
    out
}

/// Every badge summed: the stage's total, whatever the fold.
pub fn total(badges: &[Badge]) -> Tally {
    let mut total = Tally::default();
    for b in badges {
        for (t, c) in total.counts.iter_mut().zip(b.tally.counts) {
            *t += c;
        }
        for (t, c) in total.details.iter_mut().zip(b.tally.details) {
            *t += c;
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 0 (root) -> 1, 2; 1 -> 3, 4, 5; 2 -> 6; 6 -> 7. Slot 8 is not a member.
    fn tree() -> Hierarchy {
        let mut h = Hierarchy::default();
        h.set(0, None, 0);
        for (s, p) in [(1, 0), (2, 0), (3, 1), (4, 1), (5, 1), (6, 2), (7, 6)] {
            assert!(h.set(s, Some(p), u64::from(s)));
        }
        h
    }

    fn live(_: u32) -> bool {
        true
    }

    #[test]
    fn the_cut_fits_the_budget_and_folds_the_largest_parents_first() {
        let h = tree();
        // Budget 3: root plus its two children.
        let a = auto_expand(&h, live, 3);
        assert_eq!(
            a,
            vec![true, false, false, false, false, false, false, false]
        );
        // Budget 5: node 2 (one child) opens before node 1 (three), then 6 (one child).
        let a = auto_expand(&h, live, 5);
        assert!(a[0] && a[2] && a[6] && !a[1]);
        let f = resolve(&h, live, &a, &HashMap::new());
        assert_eq!(f.visible_count(), 5);
        assert_eq!(f.hidden_below(1), 3);
        assert_eq!(f.carrier(4), Some(1));
        // Enough for everything.
        let a = auto_expand(&h, live, 100);
        let f = resolve(&h, live, &a, &HashMap::new());
        assert_eq!((f.visible_count(), f.hidden_count()), (8, 0));
        assert!(!f.is_hidden(8), "a non-member is never hidden");
    }

    /// Distinguishes ascending from descending order: with room for 3 more, ascending
    /// opens the 1-child parent and then cannot fit the 3-child one; descending would
    /// open the 3-child parent first. Ranks are set against the count order, so a cut by
    /// rank alone opens the other one too.
    #[test]
    fn within_a_level_the_smallest_parent_opens_first_whatever_its_rank() {
        let mut h = Hierarchy::default();
        h.set(0, None, 0);
        h.set(1, Some(0), 1); // three children; the lower rank would put it first
        h.set(2, Some(0), 9); // one child
        for s in 3..6 {
            h.set(s, Some(1), 0);
        }
        h.set(6, Some(2), 0);
        let a = auto_expand(&h, live, 3 + 3);
        assert!(a[0] && a[2] && !a[1], "{a:?}");
        let a = auto_expand(&h, live, 3 + 1);
        assert!(a[2] && !a[1], "{a:?}");
    }

    /// Two parents with equal child counts and room for one: the lower rank wins, even
    /// when its slot is higher (so neither slot order nor ignoring rank passes).
    #[test]
    fn ties_on_child_count_go_to_the_lower_rank() {
        let mut h = Hierarchy::default();
        h.set(0, None, 0);
        h.set(1, Some(0), 50);
        h.set(2, Some(0), 10);
        for (s, p) in [(3, 1), (4, 1), (5, 2), (6, 2)] {
            h.set(s, Some(p), 0);
        }
        let a = auto_expand(&h, live, 3 + 2);
        assert!(a[2] && !a[1], "{a:?}");
    }

    /// A shallower parent opens before a deeper one even with more children.
    #[test]
    fn a_shallower_level_opens_before_a_deeper_one() {
        let mut h = Hierarchy::default();
        h.set(0, None, 0);
        h.set(1, None, 0);
        h.set(2, Some(0), 0); // 0 has one child (2), which has one child (3)
        h.set(3, Some(2), 0);
        for s in 4..7 {
            h.set(s, Some(1), 0); // 1 has three children
        }
        // Room for 4 more: depth 0 opens 0 (1) and 1 (3) before 2 (depth 1).
        let a = auto_expand(&h, live, 2 + 4);
        assert!(a[0] && a[1] && !a[2], "{a:?}");
    }

    /// A manual node (a work node) is never expanded by the cut, whatever the room;
    /// the user's override still opens it.
    #[test]
    fn the_cut_never_expands_a_manual_node() {
        let mut h = Hierarchy::default();
        h.set(0, None, 0);
        h.set_manual(1, None, 0);
        h.set(2, Some(0), 0);
        h.set(3, Some(1), 0);
        let a = auto_expand(&h, live, 100);
        assert!(a[0] && !a[1], "{a:?}");
        let f = resolve(&h, live, &a, &HashMap::new());
        assert!(f.is_hidden(3));
        let f = resolve(&h, live, &a, &HashMap::from([(1, true)]));
        assert!(!f.is_hidden(3));
        h.remove(1);
        assert!(h.is_auto(1), "a removed slot forgets it");
    }

    #[test]
    fn roots_are_drawn_even_past_the_budget() {
        let mut h = Hierarchy::default();
        for s in 0..10 {
            h.set(s, None, 0);
        }
        h.set(10, Some(0), 0);
        let a = auto_expand(&h, live, 4);
        let f = resolve(&h, live, &a, &HashMap::new());
        assert_eq!(f.visible_count(), 10);
        assert!(f.is_hidden(10));
    }

    #[test]
    fn dead_slots_are_neither_drawn_nor_counted() {
        let h = tree();
        let alive = |s: u32| s != 3 && s != 4;
        let a = auto_expand(&h, alive, 4);
        // Root 1 + children 1, 2 = 3; then 1 has one live child (5), 2 has one (6):
        // node 1 ranks first (rank 1 < 2).
        assert!(a[0] && a[1] && !a[2]);
        let f = resolve(&h, alive, &a, &HashMap::new());
        assert_eq!(f.visible_count(), 4);
        assert!(!f.is_hidden(3), "dead, not hidden");
    }

    /// The reviewer's counterexample: 50 roots with one child each and one root with
    /// 899 children fill a budget of 1,000 exactly. Without history one more child
    /// flipped 899 drawn nodes. With the previous cut kept, it moves one.
    #[test]
    fn hysteresis_keeps_the_cut_when_one_child_tips_it_over() {
        let build = |big: u32| {
            let mut h = Hierarchy::default();
            let mut s = 0u32;
            for _ in 0..50 {
                let r = s;
                h.set(r, None, 1);
                s += 1;
                h.set(s, Some(r), 0);
                s += 1;
            }
            let r = s;
            h.set(r, None, 2);
            s += 1;
            for _ in 0..big {
                h.set(s, Some(r), 0);
                s += 1;
            }
            (h, r)
        };
        let (h, big) = build(899);
        let before = auto_expand(&h, live, 1_000);
        let f0 = resolve(&h, live, &before, &HashMap::new());
        assert_eq!(f0.visible_count(), 1_000);
        assert!(before[big as usize]);

        let (h, big) = build(900);
        let cold = resolve(&h, live, &auto_expand(&h, live, 1_000), &HashMap::new());
        let warm_cut = auto_expand_from(&h, live, 1_000, &before);
        let warm = resolve(&h, live, &warm_cut, &HashMap::new());
        assert!(warm.visible_count() <= 1_000);
        assert!(warm_cut[big as usize], "the big parent stays open");
        let changed = (0..h.len() as u32)
            .filter(|&s| s < 101 && f0.is_visible(s) != warm.is_visible(s))
            .count();
        assert_eq!(changed, 1, "one small parent folded instead");
        assert!(
            cold.visible_count() < 200,
            "without history the level flips"
        );

        // And back: removing the child keeps the cut and re-opens the small parent.
        let (h, _) = build(899);
        let back = auto_expand_from(&h, live, 1_000, &warm_cut);
        assert_eq!(
            resolve(&h, live, &back, &HashMap::new()).visible_count(),
            1_000
        );
    }

    /// Folding picks the deepest open parent first, then the smallest.
    #[test]
    fn over_budget_the_deepest_and_smallest_open_parent_folds_first() {
        let h = tree(); // 0 -> 1 (3 kids), 2 -> 6 -> 7
        let all = auto_expand(&h, live, 100);
        // Budget 7 of 8: fold 6 (depth 2, one child), not 1 or 0.
        let a = auto_expand_from(&h, live, 7, &all);
        assert!(!a[6] && a[1] && a[2] && a[0], "{a:?}");
        // Budget 4: 6, then 2 (one child) and 1 fold; the cut regrows 2: 0, 1, 2, 6.
        let a = auto_expand_from(&h, live, 4, &all);
        let f = resolve(&h, live, &a, &HashMap::new());
        assert!(f.visible_count() <= 4);
        assert!(a[0], "the root stays open");
    }

    #[test]
    fn overrides_open_and_close_without_touching_the_rest() {
        let h = tree();
        let a = auto_expand(&h, live, 3);
        let f = resolve(&h, live, &a, &HashMap::from([(1, true)]));
        assert_eq!(f.visible_count(), 6);
        assert!(f.is_hidden(6), "node 2 stays folded");
        let f = resolve(&h, live, &a, &HashMap::from([(1, true), (0, false)]));
        assert_eq!(f.visible_count(), 1, "folding the root hides all");
        assert_eq!(f.hidden_below(0), 7);
        assert_eq!(f.carrier(3), Some(0));
    }

    #[test]
    fn a_cycle_is_refused() {
        let mut h = tree();
        assert!(!h.set(0, Some(7), 0));
        assert!(!h.set(1, Some(1), 0));
        assert!(!h.set(2, Some(6), 0), "6 is below 2");
        assert_eq!(h.parent(0), None);
        assert_eq!(h.parent(1), Some(0));
        assert_eq!(h.parent(2), Some(0));
    }

    /// The reviewer measured 1.33 s to build a 20,000-deep chain (a walk to the root on
    /// every set). Placed top down, each node is a leaf when set: O(1) each.
    #[test]
    fn a_deep_chain_builds_in_linear_time() {
        let t = std::time::Instant::now();
        let mut h = Hierarchy::default();
        h.set(0, None, 0);
        for s in 1..20_000u32 {
            assert!(h.set(s, Some(s - 1), 0));
        }
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        assert!(ms < 200.0, "{ms:.0} ms");
        assert!(!h.set(0, Some(19_999), 0), "the cycle is still refused");
    }

    #[test]
    fn remove_returns_orphans_as_roots() {
        let mut h = tree();
        let g = h.generation();
        let mut orphans = h.remove(1);
        orphans.sort_unstable();
        assert_eq!(orphans, vec![3, 4, 5]);
        assert_eq!(h.parent(3), None);
        assert_eq!(h.children(0), &[2]);
        assert!(!h.is_member(1));
        assert!(h.generation() > g);
        let g = h.generation();
        h.set(2, Some(0), 2);
        assert_eq!(h.generation(), g, "an unchanged set is not a change");
    }

    #[test]
    fn groups_bound_every_fanout_and_cover_each_item_once() {
        assert!(plan_groups(64, 64).is_empty());
        for n in [65usize, 200, 4_096, 50_000] {
            let g = plan_groups(n, 64);
            let top = g.iter().filter(|x| x.parent.is_none()).count();
            assert!(top <= 64, "{n}: {top} top groups");
            for (i, x) in g.iter().enumerate() {
                let kids = g.iter().filter(|y| y.parent == Some(i)).count();
                if x.leaf {
                    assert!(x.range.len() <= 64, "{n}: leaf of {}", x.range.len());
                    assert_eq!(kids, 0);
                } else {
                    assert!((2..=64).contains(&kids), "{n}: {kids} subgroups");
                }
            }
            let mut covered = vec![0u8; n];
            for x in g.iter().filter(|x| x.leaf) {
                for i in x.range.clone() {
                    covered[i] += 1;
                }
            }
            assert!(covered.iter().all(|&c| c == 1), "{n}");
        }
        // Stable under one insert: same groups, ranges moved by at most one.
        let a = plan_groups(200, 64);
        let b = plan_groups(201, 64);
        assert_eq!(a.len(), b.len());
        for (x, y) in a.iter().zip(&b) {
            assert_eq!(x.path, y.path);
            assert!(y.range.start.abs_diff(x.range.start) <= 1);
        }
    }

    #[test]
    fn click_expands_a_folded_node_and_a_second_click_folds_it() {
        let h = tree();
        let a = auto_expand(&h, live, 3);
        let f = resolve(&h, live, &a, &HashMap::new());
        assert_eq!(on_click(&f, Some(1), false), Some(Toggle::Expand(1)));
        assert_eq!(on_click(&f, Some(1), true), Some(Toggle::Expand(1)));
        let f = resolve(&h, live, &a, &HashMap::from([(1, true)]));
        assert_eq!(on_click(&f, Some(1), false), None, "selects only");
        assert_eq!(on_click(&f, Some(1), true), Some(Toggle::Collapse(1)));
        assert_eq!(on_click(&f, Some(3), true), None, "a leaf never toggles");
        assert_eq!(on_click(&f, None, false), None);
    }

    fn at(f: &Fold) -> impl Fn(u32) -> Option<Carrier> + '_ {
        move |s: u32| Some(Carrier::Node(f.carrier(s).unwrap_or(s)))
    }

    #[test]
    fn notices_stack_on_the_folded_parent_and_split_on_expand() {
        let h = tree();
        let n = |slot, kind, detail| Notice { slot, kind, detail };
        let notices = [
            n(3, NoticeKind::NeedsInput, Detail::Approval),
            n(4, NoticeKind::NeedsInput, Detail::Approval),
            n(1, NoticeKind::Failed, Detail::FailedGate),
            n(7, NoticeKind::Failed, Detail::StoppedSchedule),
            n(5, NoticeKind::Working, Detail::Working),
        ];
        let a = auto_expand(&h, live, 3);
        let folded = resolve(&h, live, &a, &HashMap::new());
        let badges = aggregate(&notices, at(&folded));
        assert_eq!(badges.len(), 2);
        let b1 = &badges[0];
        assert_eq!(b1.at, Carrier::Node(1));
        assert_eq!(b1.tally.counts, [1, 2, 1, 0]);
        assert_eq!(b1.tally.attention(), 3, "working is data, not counted");
        assert_eq!(b1.tally.most_urgent(), Some(NoticeKind::Failed));
        assert_eq!(b1.tally.words(), "1 failed gate, 2 approvals");
        assert_eq!(b1.sources.len(), 4);
        assert_eq!(badges[1].at, Carrier::Node(2));
        assert_eq!(badges[1].tally.most_urgent(), Some(NoticeKind::Failed));

        let open = resolve(&h, live, &a, &HashMap::from([(1, true)]));
        let badges = aggregate(&notices, at(&open));
        let on = |s: u32| badges.iter().find(|b| b.at == Carrier::Node(s));
        assert_eq!(on(1).map(|b| b.tally.attention()), Some(1));
        assert_eq!(on(3).map(|b| b.tally.words()), Some("1 approval".into()));
        assert_eq!(on(5).map(|b| b.tally.attention()), Some(0));
        assert_eq!(total(&badges), total(&aggregate(&notices, at(&folded))));
        assert_eq!(total(&badges).attention(), 4);
    }

    #[test]
    fn notices_on_dead_slots_are_dropped_and_groups_carry_their_own() {
        let notices = [Notice {
            slot: 9,
            kind: NoticeKind::NeedsInput,
            detail: Detail::HeldForYou,
        }];
        assert!(aggregate(&notices, |_| None).is_empty());
        let g = aggregate(&notices, |_| Some(Carrier::Group(3)));
        assert_eq!(g[0].at, Carrier::Group(3));
        assert_eq!(total(&[]).attention(), 0);
        assert_eq!(total(&g).words(), "1 held for you");
        assert_eq!(Tally::default().most_urgent(), None);
        assert_eq!(Tally::default().words(), "");
    }

    #[test]
    fn a_folded_proxy_grows_with_what_it_hides_up_to_the_cap() {
        let h = tree();
        let a = auto_expand(&h, live, 3);
        let f = resolve(&h, live, &a, &HashMap::new());
        assert!((f.scale(1) - 2.0).abs() < 1e-6, "sqrt(1 + 3)");
        assert_eq!(f.scale(0), 1.0, "expanded");
        let mut star = Hierarchy::default();
        star.set(0, None, 0);
        for s in 1..=20 {
            star.set(s, Some(0), 0);
        }
        let f = resolve(&star, live, &auto_expand(&star, live, 1), &HashMap::new());
        assert_eq!(f.hidden_below(0), 20);
        assert_eq!(f.scale(0), FOLD_SCALE_MAX, "sqrt(21) capped");
    }
}
