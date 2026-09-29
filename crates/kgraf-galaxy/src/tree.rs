//! The fold hierarchy of a project (decision 0068), kept current as code changes.
//!
//! The logical tree:
//! - a chunk under its `contains` parent. The indexer writes one per chunk; if two
//!   arrive, the **last one wins** (and a removed one hands the child back to its
//!   directory until a new one arrives);
//! - a file (or any chunk without a `contains` parent) under its directory;
//! - a directory under its parent directory. A directory with no items and exactly one
//!   subdirectory is merged into it, so `crates/app/src` is one node when `crates/app`
//!   holds nothing else. Top-level directories and root files sit under an invisible root.
//!
//! Each directory is a **proxy** node in the scene (a gray ring, `Population::Cluster`).
//! Any parent with more than [`MAX_FANOUT`] children, the root included, splits them
//! into **group proxies** ([`crate::fold::plan_groups`]): contiguous ranges of the
//! children sorted by label, so every node has at most [`MAX_FANOUT`] children.
//!
//! Proxy positions are pure functions of their keys (0058): a directory at its
//! cluster's ring center (plus a hashed offset inside the cluster when it is deeper
//! than the cluster key), a group beside its parent. They never move while they exist.
//!
//! Upkeep is incremental: a change marks the parents whose child lists changed, and
//! [`ProjectTree::sync`] re-presents only those. The directory skeleton is rebuilt
//! (O(directories + top-level items)) only when a directory gains its first item or
//! loses its last.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use kgraf_graph::layout::{CLUSTER_RADIUS, cluster_center, cluster_key, directory_of};

use crate::error::GalaxyError;
use crate::fold::{Hierarchy, MAX_FANOUT, plan_groups};
use crate::project::{CLUSTER_LIFT, CLUSTER_SIZE, SceneEdit, fnv64};
use crate::scene::{NodeInput, Population};

/// Height of a group proxy above its parent.
pub const GROUP_LIFT: f32 = 20.0;
/// Radius of a group proxy's hashed offset around its parent.
pub const GROUP_SPREAD: f32 = 30.0;

/// What a proxy stands for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProxyKind {
    /// A directory (after merging single-child chains).
    Directory,
    /// A range of a parent's children.
    Group,
}

/// A proxy's identity and names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyInfo {
    /// The fold key: `dir:<path>` or `grp:<parent key>:<path>`.
    pub key: String,
    pub kind: ProxyKind,
    /// The short name (STG-DRAW-10): the last directory component, or "N items".
    pub short: String,
    /// The full name: the directory path, or "first ... last".
    pub full: String,
}

/// A logical parent.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Parent {
    Root,
    Dir(String),
    Code(u32),
}

/// A code node as the tree keeps it: no strings. Its key and sort label stay in the
/// model and are read through [`CodeNames`] only where a parent has more children than
/// the fan-out and they must be sorted into groups. (Copying three strings per node
/// cost about 8 ms of a 100,000-node build, 2026-09-27.)
#[derive(Debug, Clone, Copy)]
struct Item {
    /// The directory of its file, interned.
    dir: u32,
    /// Its rank: a hash of its graph id.
    rank: u64,
}

/// The model's names for code nodes, read when sorting a long child list.
pub trait CodeNames {
    /// How a code node sorts among its siblings.
    fn label(&self, slot: u32) -> &str;
    /// Its graph id.
    fn key(&self, slot: u32) -> &str;
}

/// Code items by slot: a vector, since slots are dense (a map cost a hash per sort
/// comparison when presenting large child lists).
#[derive(Debug, Default)]
struct Items(Vec<Option<Item>>);

impl Items {
    fn get(&self, slot: &u32) -> Option<&Item> {
        self.0.get(*slot as usize).and_then(Option::as_ref)
    }
    fn contains_key(&self, slot: &u32) -> bool {
        self.get(slot).is_some()
    }
    fn insert(&mut self, slot: u32, item: Item) {
        let i = slot as usize;
        if self.0.len() <= i {
            self.0.resize_with(i + 1, || None);
        }
        self.0[i] = Some(item);
    }
    fn remove(&mut self, slot: &u32) {
        if let Some(x) = self.0.get_mut(*slot as usize) {
            *x = None;
        }
    }
}

/// A slot-indexed map: slots are dense, so a vector beats a hash map here.
#[derive(Debug)]
struct Dense<T>(Vec<Option<T>>);

impl<T> Default for Dense<T> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl<T> Dense<T> {
    fn get(&self, slot: &u32) -> Option<&T> {
        self.0.get(*slot as usize).and_then(Option::as_ref)
    }
    fn get_mut(&mut self, slot: &u32) -> Option<&mut T> {
        self.0.get_mut(*slot as usize).and_then(Option::as_mut)
    }
    fn contains_key(&self, slot: &u32) -> bool {
        self.get(slot).is_some()
    }
    fn insert(&mut self, slot: u32, value: T) -> Option<T> {
        let i = slot as usize;
        if self.0.len() <= i {
            self.0.resize_with(i + 1, || None);
        }
        self.0[i].replace(value)
    }
    fn remove(&mut self, slot: &u32) -> Option<T> {
        self.0.get_mut(*slot as usize).and_then(Option::take)
    }
    fn entry_or_default(&mut self, slot: u32) -> &mut T
    where
        T: Default,
    {
        let i = slot as usize;
        if self.0.len() <= i {
            self.0.resize_with(i + 1, || None);
        }
        self.0[i].get_or_insert_with(T::default)
    }
    fn keys(&self) -> impl Iterator<Item = u32> + '_ {
        self.0
            .iter()
            .enumerate()
            .filter(|(_, v)| v.is_some())
            .map(|(i, _)| i as u32)
    }
}

/// The project's fold hierarchy and its proxies.
#[derive(Debug, Default)]
pub struct ProjectTree {
    hierarchy: Hierarchy,
    items: Items,
    /// Chunk to its `contains` parent (last wins), and the reverse.
    contains_parent: Dense<u32>,
    contains_children: Dense<Vec<u32>>,
    /// Items without a `contains` parent, by raw directory.
    dir_items: BTreeMap<String, BTreeSet<u32>>,
    /// Directory node path to its parent (None for the root) and its proxy slot.
    dirs: BTreeMap<String, (Option<String>, u32)>,
    /// Directory nodes by parent (None for the root).
    dir_children: HashMap<Option<String>, BTreeSet<String>>,
    /// Groups by key: slot. And each parent's group keys.
    groups: HashMap<String, u32>,
    groups_of: HashMap<Parent, Vec<String>>,
    info: HashMap<u32, ProxyInfo>,
    by_key: HashMap<String, u32>,
    skeleton_dirty: bool,
    dirty: BTreeSet<Parent>,
    /// `contains` edges refused because they would close a cycle (the child stays
    /// under its directory). Shown by the view: never silent.
    refused_contains: usize,
    /// Work-node parents refused because they would close a cycle (the node goes to
    /// the top). Counted apart: they come from the plan, not the index.
    refused_work: usize,
    /// Loading a whole snapshot: changes are not marked one by one; [`Self::load`]
    /// marks every parent once at the end.
    bulk: bool,
    /// The color new proxies get (the model's palette).
    proxy_color: [f32; 3],
    /// Interned directory paths: an item stores the index.
    dir_names: Vec<String>,
    dir_index: HashMap<String, u32>,
}

impl ProjectTree {
    pub fn hierarchy(&self) -> &Hierarchy {
        &self.hierarchy
    }

    /// The color new proxies get.
    pub fn set_proxy_color(&mut self, color: [f32; 3]) {
        self.proxy_color = color;
    }

    /// Places a work node (a plan node) under `parent`, another work node, or at the
    /// top. Work nodes are manual members: the automatic cut never expands them, so a
    /// work parent stays folded unless the user expands it (decision 0068). A parent
    /// that would close a cycle is refused and counted; the node goes to the top.
    pub fn set_work(&mut self, slot: u32, parent: Option<u32>, key: &str) {
        let rank = fnv64(key);
        if !self.hierarchy.set_manual(slot, parent, rank) {
            self.refused_work += 1;
            self.hierarchy.set_manual(slot, None, rank);
        }
    }

    /// Takes a work node out of the hierarchy (before its slot is freed or it stops
    /// being a plan node). Its children become top-level until placed again.
    pub fn remove_work(&mut self, slot: u32) {
        if !self.items.contains_key(&slot) && !self.info.contains_key(&slot) {
            self.hierarchy.remove(slot);
        }
    }

    /// The proxy at `slot`, if one is.
    pub fn proxy(&self, slot: u32) -> Option<&ProxyInfo> {
        self.info.get(&slot)
    }

    /// Every proxy and its slot.
    pub fn proxies(&self) -> impl Iterator<Item = (u32, &ProxyInfo)> {
        self.info.iter().map(|(s, i)| (*s, i))
    }

    pub fn proxy_count(&self) -> usize {
        self.info.len()
    }

    /// The slot of a proxy key.
    pub fn proxy_slot(&self, key: &str) -> Option<u32> {
        self.by_key.get(key).copied()
    }

    /// `contains` edges refused as cycles so far.
    pub fn refused_contains(&self) -> usize {
        self.refused_contains
    }

    /// Work-node parents refused as cycles so far.
    pub fn refused_work(&self) -> usize {
        self.refused_work
    }

    /// Takes a work node to the top before its parent changes, so a set of changed
    /// parents is placed without passing through a state that looks like a cycle.
    pub fn detach_work(&mut self, slot: u32, key: &str) {
        self.hierarchy.set_manual(slot, None, fnv64(key));
    }

    /// Loads a whole snapshot at once: `nodes` as (slot, key, path) and `contains` as
    /// (parent, child). Parents are registered before items, so no item passes through
    /// its directory on the way to its parent, and every parent is marked once. Then
    /// call [`Self::sync`].
    pub fn load<'a>(
        &mut self,
        nodes: impl Iterator<Item = (u32, &'a str, &'a str)>,
        contains: &[(u32, u32)],
    ) {
        self.bulk = true;
        for &(parent, child) in contains {
            self.contains_added(parent, child);
        }
        for (slot, key, path) in nodes {
            self.code_added(slot, key, path);
        }
        self.bulk = false;
        self.skeleton_dirty = true;
        let parents: Vec<u32> = self.contains_children.keys().collect();
        self.dirty.extend(parents.into_iter().map(Parent::Code));
        // The skeleton rebuild marks every directory and the root.
    }

    fn intern(&mut self, dir: &str) -> u32 {
        if let Some(&i) = self.dir_index.get(dir) {
            return i;
        }
        let i = self.dir_names.len() as u32;
        self.dir_names.push(dir.to_owned());
        self.dir_index.insert(dir.to_owned(), i);
        i
    }

    /// The directory of a code slot.
    fn dir_of(&self, slot: u32) -> Option<String> {
        self.items
            .get(&slot)
            .map(|i| self.dir_names[i.dir as usize].clone())
    }

    /// A code node appeared.
    pub fn code_added(&mut self, slot: u32, key: &str, path: &str) {
        let dir = directory_of(path);
        let top = !self.contains_parent.contains_key(&slot);
        if top {
            self.add_top(slot, dir);
        }
        let dir = self.intern(dir);
        self.items.insert(
            slot,
            Item {
                dir,
                rank: fnv64(key),
            },
        );
    }

    /// A code node's label changed (its siblings re-sort).
    pub fn code_relabeled(&mut self, slot: u32) {
        if let Some(p) = self.parent_of(slot) {
            self.dirty.insert(p);
        }
    }

    /// A code node left. Its `contains` children go back to their directories.
    pub fn code_removed(&mut self, slot: u32) {
        if let Some(p) = self.parent_of(slot) {
            self.dirty.insert(p);
        }
        if let Some(p) = self.contains_parent.remove(&slot) {
            if let Some(set) = self.contains_children.get_mut(&p) {
                set.retain(|&c| c != slot);
            }
        } else if let Some(dir) = self.dir_of(slot) {
            self.remove_top(slot, &dir);
        }
        if let Some(kids) = self.contains_children.remove(&slot) {
            for k in kids {
                self.contains_parent.remove(&k);
                if let Some(dir) = self.dir_of(k) {
                    self.add_top(k, &dir);
                }
            }
        }
        self.items.remove(&slot);
        self.hierarchy.remove(slot);
        // Its groups (if any) leave on the next sync: the parent no longer exists.
        self.dirty.insert(Parent::Code(slot));
    }

    /// A `contains` edge arrived: `child` moves under `parent` (the last one wins).
    pub fn contains_added(&mut self, parent: u32, child: u32) {
        if parent == child {
            self.refused_contains += 1;
            return;
        }
        match self.contains_parent.insert(child, parent) {
            Some(old) if old == parent => return,
            Some(old) => {
                if let Some(set) = self.contains_children.get_mut(&old) {
                    set.retain(|&c| c != child);
                }
                if !self.bulk {
                    self.dirty.insert(Parent::Code(old));
                }
            }
            None => {
                if let Some(dir) = self.dir_of(child) {
                    self.remove_top(child, &dir);
                }
            }
        }
        self.contains_children.entry_or_default(parent).push(child);
        if !self.bulk {
            self.dirty.insert(Parent::Code(parent));
        }
    }

    /// A `contains` edge left: `child` goes back to its directory unless another
    /// parent has replaced this one.
    pub fn contains_removed(&mut self, parent: u32, child: u32) {
        if self.contains_parent.get(&child) != Some(&parent) {
            return;
        }
        self.contains_parent.remove(&child);
        if let Some(set) = self.contains_children.get_mut(&parent) {
            set.retain(|&c| c != child);
        }
        self.dirty.insert(Parent::Code(parent));
        if let Some(dir) = self.dir_of(child) {
            self.add_top(child, &dir);
        }
    }

    fn add_top(&mut self, slot: u32, dir: &str) {
        match self.dir_items.get_mut(dir) {
            Some(set) => {
                set.insert(slot);
            }
            None => {
                self.dir_items
                    .insert(dir.to_owned(), BTreeSet::from([slot]));
                self.skeleton_dirty = true;
            }
        }
        if !self.bulk {
            self.dirty.insert(self.dir_parent(dir));
        }
    }

    fn remove_top(&mut self, slot: u32, dir: &str) {
        if let Some(set) = self.dir_items.get_mut(dir) {
            set.remove(&slot);
            if set.is_empty() {
                self.dir_items.remove(dir);
                self.skeleton_dirty = true;
            }
        }
        self.dirty.insert(self.dir_parent(dir));
    }

    /// The logical parent of top-level items in raw directory `dir`.
    fn dir_parent(&self, dir: &str) -> Parent {
        if dir.is_empty() {
            Parent::Root
        } else {
            Parent::Dir(dir.to_owned())
        }
    }

    fn parent_of(&self, slot: u32) -> Option<Parent> {
        if let Some(&p) = self.contains_parent.get(&slot) {
            return Some(Parent::Code(p));
        }
        self.items
            .get(&slot)
            .map(|i| self.dir_parent(&self.dir_names[i.dir as usize]))
    }

    /// Applies every pending change: the directory skeleton if needed, then the child
    /// lists of the marked parents, inserting and removing proxies in `target`.
    pub fn sync(
        &mut self,
        target: &mut impl SceneEdit,
        names: &dyn CodeNames,
    ) -> Result<(), GalaxyError> {
        if std::mem::take(&mut self.skeleton_dirty) {
            self.rebuild_skeleton(target)?;
        }
        let dirty: Vec<Parent> = std::mem::take(&mut self.dirty).into_iter().collect();
        for p in dirty {
            self.present(target, &p, names)?;
        }
        Ok(())
    }

    /// Directory nodes from the directories that hold items: every ancestor, merged
    /// where a directory has no items and exactly one subdirectory.
    fn rebuild_skeleton(&mut self, target: &mut impl SceneEdit) -> Result<(), GalaxyError> {
        let mut all: BTreeSet<String> = BTreeSet::new();
        for d in self.dir_items.keys() {
            let mut at = d.as_str();
            while !at.is_empty() && all.insert(at.to_owned()) {
                at = directory_of(at);
            }
        }
        let mut subdirs: HashMap<&str, usize> = HashMap::new();
        for d in &all {
            *subdirs.entry(directory_of(d)).or_default() += 1;
        }
        let is_node =
            |d: &str| self.dir_items.contains_key(d) || subdirs.get(d).copied().unwrap_or(0) != 1;
        let mut want: BTreeMap<String, Option<String>> = BTreeMap::new();
        for d in &all {
            if !is_node(d) {
                continue;
            }
            let mut up = directory_of(d);
            while !up.is_empty() && !is_node(up) {
                up = directory_of(up);
            }
            want.insert(d.clone(), (!up.is_empty()).then(|| up.to_owned()));
        }
        // Remove directory proxies that left; their children are re-presented below.
        let gone: Vec<String> = self
            .dirs
            .keys()
            .filter(|d| !want.contains_key(*d))
            .cloned()
            .collect();
        let as_parent = |p: &Option<String>| match p {
            None => Parent::Root,
            Some(d) => Parent::Dir(d.clone()),
        };
        for d in gone {
            if let Some((parent, slot)) = self.dirs.remove(&d) {
                if let Some(set) = self.dir_children.get_mut(&parent) {
                    set.remove(&d);
                }
                self.dirty.insert(as_parent(&parent));
                self.drop_groups(target, &Parent::Dir(d.clone()))?;
                self.drop_proxy(target, slot)?;
            }
        }
        // Only the child lists that changed are marked: a directory's items do not move
        // with the skeleton, only which directory nodes sit under which.
        for (d, parent) in want {
            match self.dirs.get_mut(&d) {
                Some(entry) => {
                    if entry.0 != parent {
                        let old = std::mem::replace(&mut entry.0, parent.clone());
                        if let Some(set) = self.dir_children.get_mut(&old) {
                            set.remove(&d);
                        }
                        self.dir_children
                            .entry(parent.clone())
                            .or_default()
                            .insert(d.clone());
                        self.dirty.insert(as_parent(&old));
                        self.dirty.insert(as_parent(&parent));
                    }
                }
                None => {
                    let key = format!("dir:{d}");
                    let slot = self.add_proxy(
                        target,
                        dir_position(&d),
                        ProxyInfo {
                            key: key.clone(),
                            kind: ProxyKind::Directory,
                            short: d.rsplit('/').next().unwrap_or(&d).to_owned(),
                            full: d.clone(),
                        },
                    )?;
                    self.dir_children
                        .entry(parent.clone())
                        .or_default()
                        .insert(d.clone());
                    self.dirty.insert(as_parent(&parent));
                    self.dirty.insert(Parent::Dir(d.clone()));
                    self.dirs.insert(d.clone(), (parent, slot));
                }
            }
        }
        Ok(())
    }

    fn add_proxy(
        &mut self,
        target: &mut impl SceneEdit,
        position: [f32; 3],
        info: ProxyInfo,
    ) -> Result<u32, GalaxyError> {
        let slot = target.insert_node(
            NodeInput {
                position,
                color: self.proxy_color,
                size: CLUSTER_SIZE,
            },
            Population::Cluster,
        )?;
        self.by_key.insert(info.key.clone(), slot);
        self.info.insert(slot, info);
        Ok(slot)
    }

    fn drop_proxy(&mut self, target: &mut impl SceneEdit, slot: u32) -> Result<(), GalaxyError> {
        if let Some(info) = self.info.remove(&slot) {
            self.by_key.remove(&info.key);
            self.groups.remove(&info.key);
        }
        self.hierarchy.remove(slot);
        target.remove_node(slot)
    }

    fn drop_groups(&mut self, target: &mut impl SceneEdit, p: &Parent) -> Result<(), GalaxyError> {
        for key in self.groups_of.remove(p).unwrap_or_default() {
            if let Some(slot) = self.groups.remove(&key) {
                self.drop_proxy(target, slot)?;
            }
        }
        Ok(())
    }

    /// The slot a logical parent is drawn as (`None` for the root; `Err` when it no
    /// longer exists).
    fn slot_of_parent(&self, p: &Parent) -> Result<Option<u32>, ()> {
        match p {
            Parent::Root => Ok(None),
            Parent::Dir(d) => self.dirs.get(d).map(|(_, s)| Some(*s)).ok_or(()),
            Parent::Code(s) => {
                if self.items.contains_key(s) {
                    Ok(Some(*s))
                } else {
                    Err(())
                }
            }
        }
    }

    fn key_of_parent(&self, p: &Parent, names: &dyn CodeNames) -> String {
        match p {
            Parent::Root => "root".to_owned(),
            Parent::Dir(d) => format!("dir:{d}"),
            Parent::Code(s) => names.key(*s).to_owned(),
        }
    }

    /// A child's sort label and key, borrowed.
    fn sort_key<'a>(&'a self, slot: u32, names: &'a dyn CodeNames) -> (&'a str, &'a str) {
        if self.items.contains_key(&slot) {
            return (names.label(slot), names.key(slot));
        }
        self.info
            .get(&slot)
            .map_or(("", ""), |i| (i.full.as_str(), i.key.as_str()))
    }

    /// A child's rank: its own for code, a hash of its key for a proxy.
    fn rank_of(&self, slot: u32) -> u64 {
        match self.items.get(&slot) {
            Some(i) => i.rank,
            None => self.info.get(&slot).map_or(0, |i| fnv64(&i.key)),
        }
    }

    /// The logical children of `p`, as slots.
    fn children_of(&self, p: &Parent) -> Vec<u32> {
        let mut out = Vec::new();
        match p {
            Parent::Code(s) => {
                if let Some(set) = self.contains_children.get(s) {
                    out.extend(set.iter().copied());
                }
            }
            Parent::Root | Parent::Dir(_) => {
                let me = match p {
                    Parent::Dir(d) => Some(d.clone()),
                    _ => None,
                };
                if let Some(subs) = self.dir_children.get(&me) {
                    out.extend(
                        subs.iter()
                            .filter_map(|d| self.dirs.get(d).map(|(_, s)| *s)),
                    );
                }
                if let Some(set) = self.dir_items.get(me.as_deref().unwrap_or("")) {
                    out.extend(set.iter().copied());
                }
            }
        }
        out
    }

    fn set_parent(&mut self, slot: u32, parent: Option<u32>, rank: u64) {
        if !self.hierarchy.set(slot, parent, rank) {
            // A `contains` cycle in the data: the child stays under its directory.
            self.refused_contains += 1;
            let home = self
                .dir_of(slot)
                .and_then(|d| self.dirs.get(&d))
                .map(|(_, s)| *s);
            self.hierarchy.set(slot, home, rank);
        }
    }

    /// Re-presents one parent's children: directly when they fit the fan-out (no
    /// sorting, no names read), else sorted by label under group proxies.
    fn present(
        &mut self,
        target: &mut impl SceneEdit,
        p: &Parent,
        names: &dyn CodeNames,
    ) -> Result<(), GalaxyError> {
        let Ok(parent_slot) = self.slot_of_parent(p) else {
            return self.drop_groups(target, p);
        };
        let mut kids = self.children_of(p);
        if kids.len() <= MAX_FANOUT {
            for slot in kids {
                let rank = self.rank_of(slot);
                self.set_parent(slot, parent_slot, rank);
            }
            if self.groups_of.contains_key(p) {
                return self.drop_groups(target, p);
            }
            return Ok(());
        }
        kids.sort_by(|&a, &b| {
            self.sort_key(a, names)
                .cmp(&self.sort_key(b, names))
                .then(a.cmp(&b))
        });
        let ranked: Vec<(u32, u64)> = kids.iter().map(|&s| (s, self.rank_of(s))).collect();
        let plan = plan_groups(kids.len(), MAX_FANOUT);
        let pkey = self.key_of_parent(p, names);
        let base = match parent_slot {
            Some(s) => target.scene().nodes().get(s as usize).map(|n| n.position),
            None => None,
        };
        let mut keys = Vec::with_capacity(plan.len());
        let mut slots = Vec::with_capacity(plan.len());
        for g in &plan {
            let path: Vec<String> = g.path.iter().map(u16::to_string).collect();
            let key = format!("grp:{pkey}:{}", path.join("."));
            let first = short_label(self.sort_key(kids[g.range.start], names).0).to_owned();
            let last = short_label(self.sort_key(kids[g.range.end - 1], names).0).to_owned();
            let info = ProxyInfo {
                key: key.clone(),
                kind: ProxyKind::Group,
                short: format!("{} items", g.range.len()),
                full: format!("{first} ... {last}"),
            };
            let slot = match self.groups.get(&key) {
                Some(&s) => {
                    self.info.insert(s, info);
                    s
                }
                None => {
                    let s = self.add_proxy(target, group_position(base, &key), info)?;
                    self.groups.insert(key.clone(), s);
                    s
                }
            };
            let parent = match g.parent {
                Some(i) => Some(slots[i]),
                None => parent_slot,
            };
            self.set_parent(slot, parent, fnv64(&key));
            keys.push(key);
            slots.push(slot);
        }
        for (i, g) in plan.iter().enumerate() {
            if g.leaf {
                for &(slot, rank) in &ranked[g.range.clone()] {
                    self.set_parent(slot, Some(slots[i]), rank);
                }
            }
        }
        let old = self
            .groups_of
            .insert(p.clone(), keys.clone())
            .unwrap_or_default();
        let keep: HashSet<&String> = keys.iter().collect();
        for key in old.iter().filter(|k| !keep.contains(k)) {
            if let Some(slot) = self.groups.remove(key) {
                self.drop_proxy(target, slot)?;
            }
        }
        Ok(())
    }
}

/// The last `::` or `/` segment of a label.
fn short_label(label: &str) -> &str {
    label
        .rsplit("::")
        .next()
        .and_then(|l| l.rsplit('/').next())
        .unwrap_or(label)
}

/// A point in a disc of `radius` hashed from `key`.
fn disc_offset(key: &str, radius: f32) -> [f32; 2] {
    let h = fnv64(key);
    let angle = (h & 0xFFFF) as f32 / 65535.0 * std::f32::consts::TAU;
    let r = (((h >> 16) & 0xFFFF) as f32 / 65535.0).sqrt() * radius;
    [r * angle.cos(), r * angle.sin()]
}

/// A directory proxy's position: its cluster's ring center, offset inside the cluster
/// when the directory is deeper than the cluster key.
fn dir_position(dir: &str) -> [f32; 3] {
    let (x, y) = cluster_center(dir);
    let [dx, dy] = if cluster_key(dir) == dir {
        [0.0, 0.0]
    } else {
        disc_offset(dir, CLUSTER_RADIUS)
    };
    [x + dx, y + dy, CLUSTER_LIFT]
}

/// A group proxy's position: beside its parent (or on the ring for a root group).
fn group_position(parent: Option<[f32; 3]>, key: &str) -> [f32; 3] {
    let [dx, dy] = disc_offset(key, GROUP_SPREAD);
    match parent {
        Some([x, y, z]) => [x + dx, y + dy, z + GROUP_LIFT],
        None => {
            let (x, y) = cluster_center(key);
            [x + dx, y + dy, CLUSTER_LIFT]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fold::{VISIBLE_BUDGET, auto_expand, resolve};
    use crate::scene::{Scene, SceneInput};

    fn scene() -> Scene {
        Scene::new(SceneInput::default()).expect("scene")
    }

    /// Test names: each slot's key is its label.
    #[derive(Default)]
    struct Names(HashMap<u32, String>);

    impl CodeNames for Names {
        fn label(&self, slot: u32) -> &str {
            self.0.get(&slot).map_or("", String::as_str)
        }
        fn key(&self, slot: u32) -> &str {
            self.label(slot)
        }
    }

    thread_local! {
        static NAMES: std::cell::RefCell<Names> = std::cell::RefCell::new(Names::default());
    }

    /// Syncs with the names recorded by [`code`].
    fn sync(t: &mut ProjectTree, s: &mut Scene) {
        NAMES.with(|n| t.sync(s, &*n.borrow())).expect("sync");
    }

    /// Adds a code node to both the scene and the tree.
    fn code(t: &mut ProjectTree, s: &mut Scene, key: &str, path: &str) -> u32 {
        let slot = s
            .insert_node(
                NodeInput {
                    position: [0.0; 3],
                    color: [1.0; 3],
                    size: 4.0,
                },
                Population::Code,
            )
            .expect("node");
        t.code_added(slot, key, path);
        NAMES.with(|n| n.borrow_mut().0.insert(slot, key.to_owned()));
        slot
    }

    #[test]
    fn single_child_directory_chains_merge() {
        let (mut t, mut s) = (ProjectTree::default(), scene());
        let a = code(&mut t, &mut s, "a", "crates/app/src/main.rs");
        let b = code(&mut t, &mut s, "b", "crates/lib/src/lib.rs");
        let c = code(&mut t, &mut s, "c", "tools/lone/x.rs");
        sync(&mut t, &mut s);
        let mut dirs: Vec<&str> = t
            .proxies()
            .filter(|(_, i)| i.kind == ProxyKind::Directory)
            .map(|(_, i)| i.full.as_str())
            .collect();
        dirs.sort_unstable();
        assert_eq!(
            dirs,
            vec!["crates", "crates/app/src", "crates/lib/src", "tools/lone"]
        );
        let h = t.hierarchy();
        let crates = t.proxy_slot("dir:crates").expect("crates");
        let app = t.proxy_slot("dir:crates/app/src").expect("app");
        assert_eq!(h.parent(a), Some(app));
        assert_eq!(h.parent(app), Some(crates));
        assert_eq!(h.parent(crates), None);
        assert_eq!(h.parent(t.proxy_slot("dir:tools/lone").expect("t")), None);
        assert!(h.parent(b).is_some() && h.parent(c).is_some());

        // Removing the only item under tools/lone removes its proxy.
        t.code_removed(c);
        s.remove_node(c).expect("rm");
        sync(&mut t, &mut s);
        assert!(t.proxy_slot("dir:tools/lone").is_none());
        assert_eq!(t.proxy_count(), 3);
        assert_eq!(s.live_node_count(), 2 + 3);
    }

    #[test]
    fn contains_nests_chunks_and_the_last_parent_wins() {
        let (mut t, mut s) = (ProjectTree::default(), scene());
        let f = code(&mut t, &mut s, "f", "src/x.rs");
        let g = code(&mut t, &mut s, "g", "src/x.rs");
        let m = code(&mut t, &mut s, "m", "src/x.rs");
        t.contains_added(f, g);
        t.contains_added(g, m);
        sync(&mut t, &mut s);
        let h = t.hierarchy();
        assert_eq!(h.parent(m), Some(g));
        assert_eq!(h.parent(g), Some(f));
        assert_eq!(h.parent(f), t.proxy_slot("dir:src"));
        // A second parent for m: the last one wins.
        t.contains_added(f, m);
        sync(&mut t, &mut s);
        assert_eq!(t.hierarchy().parent(m), Some(f));
        // Removing the stale edge changes nothing; removing the live one re-homes m.
        t.contains_removed(g, m);
        sync(&mut t, &mut s);
        assert_eq!(t.hierarchy().parent(m), Some(f));
        t.contains_removed(f, m);
        sync(&mut t, &mut s);
        assert_eq!(t.hierarchy().parent(m), t.proxy_slot("dir:src"));
        // A cycle is refused, counted, and the child stays reachable.
        t.contains_added(m, f);
        t.contains_added(f, m);
        sync(&mut t, &mut s);
        assert!(t.refused_contains() > 0);
        assert_eq!(t.refused_work(), 0);
    }

    /// 5,000 root files drew 5,000 nodes, and expanding a parent
    /// with 50,000 children drew 50,001. Every node now has at most MAX_FANOUT children.
    #[test]
    fn fanout_is_bounded_for_many_roots_and_a_huge_parent() {
        let (mut t, mut s) = (ProjectTree::default(), scene());
        for i in 0..5_000 {
            code(&mut t, &mut s, &format!("r{i:05}"), &format!("f{i:05}.rs"));
        }
        let big = code(&mut t, &mut s, "big", "big/x.rs");
        for i in 0..50_000 {
            let c = code(&mut t, &mut s, &format!("c{i:05}"), "big/x.rs");
            t.contains_added(big, c);
        }
        sync(&mut t, &mut s);
        let h = t.hierarchy();
        let alive = |x: u32| s.is_alive(x);
        let roots = h.roots(&alive).count();
        assert!(roots <= MAX_FANOUT, "{roots} roots");
        for x in 0..h.len() as u32 {
            assert!(h.children(x).len() <= MAX_FANOUT, "slot {x}");
        }
        let f = resolve(
            h,
            alive,
            &auto_expand(h, alive, VISIBLE_BUDGET),
            &HashMap::new(),
        );
        assert!(f.visible_count() <= VISIBLE_BUDGET);
        // Expanding the big file alone adds at most MAX_FANOUT nodes.
        let ov = HashMap::from([(big, true)]);
        let open = resolve(h, alive, &auto_expand(h, alive, VISIBLE_BUDGET), &ov);
        assert!(open.visible_count() <= f.visible_count() + MAX_FANOUT);
        assert!(open.visible_count() >= f.visible_count());
        // Every code node is reachable from a root.
        assert_eq!(f.visible_count() + f.hidden_count(), s.live_node_count());
    }

    #[test]
    fn groups_keep_their_slots_under_small_changes() {
        let (mut t, mut s) = (ProjectTree::default(), scene());
        let f = code(&mut t, &mut s, "f", "src/x.rs");
        let mut kids = Vec::new();
        for i in 0..200 {
            let c = code(&mut t, &mut s, &format!("k{i:03}"), "src/x.rs");
            t.contains_added(f, c);
            kids.push(c);
        }
        sync(&mut t, &mut s);
        let before: Vec<(u32, ProxyInfo)> = {
            let mut v: Vec<_> = t
                .proxies()
                .filter(|(_, i)| i.kind == ProxyKind::Group)
                .map(|(sl, i)| (sl, i.clone()))
                .collect();
            v.sort_by_key(|(sl, _)| *sl);
            v
        };
        assert_eq!(before.len(), 4, "200 children in 4 groups");
        let extra = code(&mut t, &mut s, "k100a", "src/x.rs");
        t.contains_added(f, extra);
        sync(&mut t, &mut s);
        for (slot, info) in &before {
            assert_eq!(t.proxy(*slot).map(|i| &i.key), Some(&info.key), "same slot");
        }
        // Each child moved to a different group at most at a range boundary.
        let moved = kids
            .iter()
            .filter(|&&k| {
                let p = t.hierarchy().parent(k);
                before.iter().all(|(sl, _)| Some(*sl) != p)
            })
            .count();
        assert_eq!(moved, 0, "children stay under the same set of groups");
    }
}
