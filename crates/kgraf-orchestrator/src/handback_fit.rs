//! A hand-back's child must start with room to work.
//!
//! A worker that reaches its stop point hands its remainder back, and the
//! remainder becomes a child contract in the same worktree (decision 0057).
//! The child used to inherit everything: the parent's whole pinned slice set,
//! and an instruction that quoted the parent's whole assembled brief, which
//! itself quoted its parent's. Measured with the stop point at 30% of a
//! 32,768-token window (9,830 tokens), a child's first request held 6.7k to
//! 7.5k tokens, so one round of real work crossed the stop point and handed
//! back again, until the depth cap.
//!
//! Three rules, applied where the child is made:
//!
//! 1. **One level of wrapper.** The child's instruction quotes the chain's
//!    original instruction once ([`flatten_remainder`]), not each ancestor's
//!    brief inside the next.
//! 2. **Slices under a cap.** A child is planned like any contract (decision
//!    0057): its brief, estimated as at dispatch, fits under half the stop
//!    point. What the fixed part (the prefix and the instruction) leaves is the
//!    most tokens of pinned slices it gets, each slice counted as rendered
//!    ([`child_fit`], [`fit_slices`]). A slice that is dropped stays readable
//!    with the worker's tools.
//! 3. **No child that cannot fit.** When the fixed part alone passes the
//!    target, no choice of slices helps. The split is refused at the first
//!    hand-back, with the numbers ([`Unfit`]); at dispatch the worker is
//!    spawned to compact instead of handing back (decision 0095).

use crate::sizing::bound_tokens;
use kgraf_core::NodeId;

/// What a child's slice set may take.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ChildFit {
    /// The bounded estimate of the brief without slices, in tokens.
    pub base_tokens: u64,
    /// The most tokens of slice source the child is given.
    pub slice_cap_tokens: u64,
}

/// A child whose fixed part alone passes the plan target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Unfit {
    pub base_tokens: u64,
    pub target_tokens: u64,
}

impl std::fmt::Display for Unfit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the child could not start with room to work: its prefix and instruction alone take \
             about {} tokens, over the {}-token plan target (half the stop point), so every \
             round would end at the stop point. Raise the context window or the stop point, or \
             give the contract a shorter instruction",
            self.base_tokens, self.target_tokens
        )
    }
}

/// The room a child has. `prefix_tokens` is the route's prefix estimate and
/// `brief` the child's instruction text as the worker is given it;
/// `target_tokens` is half the stop point. The estimate is the one dispatch
/// makes (`base_tokens` in the run loop): the prefix bounded at one and a half
/// times its estimate, the instruction at one token per two bytes. The slices
/// are not part of it: their cost is measured as rendered.
pub(crate) fn child_fit(
    prefix_tokens: u64,
    brief: &str,
    target_tokens: u64,
) -> Result<ChildFit, Unfit> {
    let base_tokens = prefix_tokens.saturating_mul(3).div_ceil(2) + bound_tokens(brief);
    if base_tokens > target_tokens {
        return Err(Unfit {
            base_tokens,
            target_tokens,
        });
    }
    // The slices are measured as rendered, not bounded, so the room is the cap.
    Ok(ChildFit {
        base_tokens,
        slice_cap_tokens: target_tokens - base_tokens,
    })
}

/// The slices that fit `cap_tokens`, in their given order. A slice that does
/// not fit is skipped and a later, smaller one may still be taken, so the
/// order the plan gave is kept and the budget is used.
pub(crate) fn fit_slices(sized: &[(NodeId, u64)], cap_tokens: u64) -> Vec<NodeId> {
    let mut used = 0u64;
    let mut kept = Vec::new();
    for (node, tokens) in sized {
        if used + tokens <= cap_tokens {
            used += tokens;
            kept.push(node.clone());
        }
    }
    kept
}

/// What follows the counts in a remainder, before the instruction.
const REMAINDER_TAIL: &str = " tokens). Instruction: ";

/// The chain's original instruction, then the extra paragraphs the chain
/// carries (an interface, the verify command, a refusal), once each.
///
/// `item` is the remainder the runtime made from the parent's whole assembled
/// brief: its header, then the parent's brief text (`parent_brief`, which
/// holds the parent's instruction, and for a hand-back child that is itself a
/// remainder quoting the one before), then what dispatch appended. Rebuilt as
/// the header, `root` (the instruction the chain started from) and the
/// appended paragraphs of the parent and of its instruction, repeated ones
/// dropped: the acceptance and write lines are not repeated, since the child's
/// own brief states them. Text that is not such a remainder (a remainder the
/// worker's own handoff listed), or that does not hold what it should, is
/// returned unchanged.
///
/// A last paragraph states which hand-back this is (`level`, from 1). It keeps
/// the chain's tasks distinct, as the delegation guards' cycle rule needs (a
/// child that restates its parent's task word for word is a cycle), and tells
/// the worker its predecessors' changes are in the worktree.
pub(crate) fn flatten_remainder(
    item: &str,
    parent_brief: &str,
    parent_instruction: &str,
    root: &str,
    level: usize,
) -> String {
    let Some((header, assembled)) = item
        .strip_prefix(kgraf_runtime::REMAINDER_HEAD)
        .and_then(|rest| rest.split_once(REMAINDER_TAIL))
    else {
        return item.to_owned();
    };
    let Some(parent_extras) = assembled.strip_prefix(parent_brief) else {
        return item.to_owned();
    };
    // The paragraphs an earlier level already carried: what follows the root
    // inside the parent's own (already flattened) instruction.
    let inner = kgraf_runtime::original_instruction(parent_instruction);
    let carried = if inner == parent_instruction {
        // A planned parent carries nothing before its own instruction.
        (inner == root).then_some("")
    } else {
        inner.strip_prefix(root)
    };
    let Some(carried) = carried else {
        return item.to_owned();
    };
    let mut seen: Vec<&str> = Vec::new();
    let mut extras = String::new();
    for para in carried.split("\n\n").chain(parent_extras.split("\n\n")) {
        let para = para.trim();
        // An earlier level's marker is replaced by this level's below.
        if para.starts_with(LEVEL_MARK) {
            continue;
        }
        if !para.is_empty() && !seen.contains(&para) {
            seen.push(para);
            extras.push_str("\n\n");
            extras.push_str(para);
        }
    }
    format!(
        "{}{header}{REMAINDER_TAIL}{root}{extras}\n\n{LEVEL_MARK} {level} of this instruction: \
         the workers before you reached their context stop point, and their changes are in \
         the worktree.",
        kgraf_runtime::REMAINDER_HEAD
    )
}

/// Starts the paragraph that states a hand-back's level.
const LEVEL_MARK: &str = "Hand-back";

#[cfg(test)]
mod tests {
    use super::*;
    use kgraf_runtime::handback_remainder;

    fn node(n: &str) -> NodeId {
        NodeId::new(n)
    }

    #[test]
    fn the_cap_is_what_the_fixed_part_leaves_under_the_target() {
        // 600 prefix tokens count 900; a 2,000-byte brief counts 1,000.
        let fit = child_fit(600, &"x".repeat(2_000), 4_000).expect("fits");
        assert_eq!(fit.base_tokens, 1_900);
        assert_eq!(fit.slice_cap_tokens, 2_100);
        // The brief with its capped slices stays at the target.
        assert!(fit.base_tokens + fit.slice_cap_tokens <= 4_000);
    }

    #[test]
    fn a_child_whose_fixed_part_passes_the_target_is_unfit_and_says_the_numbers() {
        let err = child_fit(3_000, &"x".repeat(2_000), 4_000).expect_err("unfit");
        assert_eq!(err.base_tokens, 5_500);
        assert_eq!(err.target_tokens, 4_000);
        let text = err.to_string();
        assert!(text.contains("5500") && text.contains("4000"), "{text}");
        assert!(text.contains("could not start with room to work"), "{text}");
        // Exactly the target fits, with no room for slices.
        let edge = child_fit(0, &"x".repeat(8_000), 4_000).expect("edge");
        assert_eq!(edge.slice_cap_tokens, 0);
    }

    #[test]
    fn slices_are_kept_in_order_under_the_cap_and_a_smaller_later_one_is_taken() {
        let sized = [
            (node("a"), 600),
            (node("b"), 900),
            (node("c"), 300),
            (node("d"), 200),
        ];
        assert_eq!(fit_slices(&sized, 1_000), [node("a"), node("c")]);
        assert_eq!(fit_slices(&sized, 0), Vec::<NodeId>::new());
        assert_eq!(fit_slices(&sized, 10_000).len(), 4);
    }

    /// What the run loop builds: the parent's brief text is its instruction,
    /// the acceptance and the write line; dispatch appends the verify command.
    fn parent_brief(instruction: &str) -> String {
        format!("{instruction}\n\nAcceptance: tests pass\nWrite only these files: a.py")
    }
    const APPENDED: &str = "\n\nYour attempt is accepted when `pytest` passes.";

    /// One hand-back, as `resplit` makes the child's instruction.
    fn child_of(parent_instruction: &str, root: &str, level: usize) -> String {
        let assembled = format!("{}{APPENDED}", parent_brief(parent_instruction));
        let item = handback_remainder(&assembled, 11_000, 9_830);
        flatten_remainder(
            &item,
            &parent_brief(parent_instruction),
            parent_instruction,
            root,
            level,
        )
    }

    #[test]
    fn a_chain_of_remainders_wraps_the_original_once_and_does_not_grow() {
        let root = "Implement the shapes, one class per section.";
        let h1 = child_of(root, root, 1);
        let h2 = child_of(&h1, root, 2);
        let h3 = child_of(&h2, root, 3);
        let h4 = child_of(&h3, root, 4);
        for h in [&h1, &h2, &h3, &h4] {
            assert_eq!(h.matches(kgraf_runtime::REMAINDER_HEAD).count(), 1, "{h}");
            assert_eq!(h.matches("Your attempt is accepted").count(), 1, "{h}");
            assert!(h.contains(root), "{h}");
            // The acceptance and write lines are the child's own brief's.
            assert!(!h.contains("Write only these files"), "{h}");
            assert_eq!(h.matches("Hand-back ").count(), 1, "{h}");
        }
        // Each level states its own, so no child's task equals its parent's.
        assert!(h1.contains("Hand-back 1 of") && h4.contains("Hand-back 4 of"));
        assert_ne!(h2, h1);
        assert_eq!(h2.len(), h1.len(), "no growth from level 1 to 2");
        assert_eq!(h4.len(), h1.len(), "no growth from level 1 to 4");
        // The nested form each level used to build.
        let mut nested = root.to_owned();
        for _ in 0..4 {
            nested = handback_remainder(
                &format!("{}{APPENDED}", parent_brief(&nested)),
                11_000,
                9_830,
            );
        }
        assert!(
            nested.len() > 2 * h4.len(),
            "{} vs {}",
            nested.len(),
            h1.len()
        );
    }

    #[test]
    fn an_extra_paragraph_of_an_earlier_level_is_carried_once() {
        let root = "Implement the shapes.";
        let interface = "\n\nCompile-only interface `i1` binds this contract.";
        // The first hand-back's assembled brief carried an interface.
        let assembled = format!("{}{interface}{APPENDED}", parent_brief(root));
        let item = handback_remainder(&assembled, 1, 2);
        let h1 = flatten_remainder(&item, &parent_brief(root), root, root, 1);
        assert!(h1.contains("Compile-only interface `i1`"), "{h1}");
        // The child has no interface of its own; the next level keeps the one it was given.
        let h2 = child_of(&h1, root, 2);
        assert_eq!(h2.matches("Compile-only interface `i1`").count(), 1, "{h2}");
        assert_eq!(h2.matches("Your attempt is accepted").count(), 1, "{h2}");
    }

    #[test]
    fn a_handoff_item_the_worker_listed_is_not_touched() {
        let item = "Write the parser tests";
        assert_eq!(flatten_remainder(item, "brief", "orig", "orig", 1), item);
        // A remainder that does not hold the parent's brief is kept as is.
        let other = handback_remainder("something else", 1, 2);
        assert_eq!(flatten_remainder(&other, "brief", "orig", "orig", 1), other);
        // A chain whose root is not the parent's instruction is kept as is.
        let odd = handback_remainder("brief", 1, 2);
        assert_eq!(
            flatten_remainder(&odd, "brief", "orig", "elsewhere", 1),
            odd
        );
    }
}
