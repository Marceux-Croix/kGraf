//! Which tool calls of one round may overlap.
//!
//! Two tools touching the same graph node serialize;
//! independent ones overlap. Each approved call carries its [`Access`], computed
//! from its final input by the approval chain. A call waits for every **earlier**
//! call in the round whose access conflicts with its own, so conflicting calls run
//! in the order the model issued them, and a call with no earlier conflict starts
//! at once.
//!
//! Written new: the conflict relation is `kgraf-tools`' own; this is the
//! ordering over it, a dependency list per call, which is a few lines and has no
//! library worth a dependency.

use kgraf_tools::{Access, PathCase};

/// For each call, the indexes of earlier calls it must wait for.
pub fn dependencies(accesses: &[Access], case: PathCase) -> Vec<Vec<usize>> {
    accesses
        .iter()
        .enumerate()
        .map(|(j, later)| {
            accesses[..j]
                .iter()
                .enumerate()
                .filter(|(_, earlier)| earlier.conflicts_with(later, case))
                .map(|(i, _)| i)
                .collect()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: PathCase = PathCase::Sensitive;

    #[test]
    fn same_node_writers_chain_in_issue_order_and_independent_calls_wait_for_nothing() {
        let a = Access::none().write_node("n1");
        let b = Access::none().read_node("n2");
        let c = Access::none().write_node("n1");
        let d = Access::none().read_node("n1");
        let deps = dependencies(&[a, b, c, d], S);
        assert_eq!(deps, vec![vec![], vec![], vec![0], vec![0, 2]]);
    }

    #[test]
    fn an_exclusive_call_waits_for_all_before_it_and_all_after_wait_for_it() {
        let deps = dependencies(&[Access::none(), Access::exclusive(), Access::none()], S);
        assert_eq!(deps, vec![vec![], vec![0], vec![1]]);
    }
}
