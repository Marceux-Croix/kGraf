//! A verifier refusal whose compile errors lie in files the refused contract
//! does not write.
//!
//! A worker that changes a public item (a unit struct into a struct with a
//! field) breaks the callers of that item, and the compiler reports the error
//! at the caller, in a file that belongs to another contract or to nobody. A
//! worker told to touch only its own files tends to read such an error as
//! someone else's and repeats the same change. The retry instruction therefore
//! gets one amendment that quotes the error and says where it is, once per
//! contract.
//!
//! Only compiler diagnostics in the `error[...]: message` then `--> file:line`
//! form (rustc, and tools that copy it) are read. A failing test or a panic
//! says nothing about who changed what, so it is not an error of this kind.
//! Compile errors of other toolchains are not recognized and are left to the
//! ordinary retry.

use crate::run_state::WriteTarget;

/// What one refusal says about an error outside the contract's files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutsideError {
    /// The file the first outside error is in, as the compiler printed it.
    pub file: String,
    /// The first backticked name in that error's message, when it has one.
    pub symbol: Option<String>,
    /// The error's message line.
    pub message: String,
}

/// The first compile error of `refusal` that lies outside `writes`, when no
/// error of it lies inside them: an error in a file the contract writes is the
/// worker's to fix, and says nothing about a caller.
pub fn outside_compile_error(refusal: &str, writes: &[WriteTarget]) -> Option<OutsideError> {
    let mut message: Option<&str> = None;
    let mut first_outside: Option<OutsideError> = None;
    for line in refusal.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("error") {
            message = Some(trimmed);
        } else if trimmed.starts_with("warning") {
            message = None;
        } else if let Some(at) = trimmed.strip_prefix("--> ")
            && let Some(msg) = message.take()
            && let Some(file) = located_file(at)
        {
            if inside(file, writes) {
                return None;
            }
            first_outside.get_or_insert_with(|| OutsideError {
                file: file.to_owned(),
                symbol: backticked(msg),
                message: msg.to_owned(),
            });
        }
    }
    first_outside
}

/// `src/lib.rs` from `src/lib.rs:5:18`.
fn located_file(location: &str) -> Option<&str> {
    let (file, rest) = location.trim().split_once(':')?;
    (!file.is_empty() && rest.starts_with(|c: char| c.is_ascii_digit())).then_some(file)
}

fn inside(file: &str, writes: &[WriteTarget]) -> bool {
    let file = file.trim_start_matches("./");
    writes.iter().any(|w| {
        let w = w.path.trim_start_matches("./");
        file == w
            || file.ends_with(&format!("/{w}"))
            || (w.ends_with('/') && (file.starts_with(w) || file.contains(&format!("/{w}"))))
    })
}

fn backticked(message: &str) -> Option<String> {
    let start = message.find('`')? + 1;
    let len = message[start..].find('`')?;
    let name = &message[start..start + len];
    (!name.is_empty()).then(|| name.to_owned())
}

/// The amendment quoted in the retry's brief. `error` is the located error;
/// the text is written for the worker, who must keep the public shape the
/// existing callers use.
pub fn amendment(error: &OutsideError) -> String {
    let uses = match &error.symbol {
        Some(s) => format!("`{s}`"),
        None => "a public item of your files".to_owned(),
    };
    format!(
        "The previous attempt failed with a compile error: {}. The error is in `{}`, which \
         uses {uses} as the existing code defines it; keep the public shape that existing \
         callers use. That file is not yours to edit: change your own files so that it compiles \
         as it is.",
        error.message, error.file
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn writes(paths: &[&str]) -> Vec<WriteTarget> {
        paths
            .iter()
            .map(|p| WriteTarget {
                path: (*p).to_owned(),
                node: None,
            })
            .collect()
    }

    const E0423: &str = "   Compiling pricing_engine v0.1.0\n\
        error[E0423]: expected value, found struct `FlatDiscount`\n\
        \x20 --> pricing_engine/src/lib.rs:5:18\n\
        \x20  |\n\
        5  |     let _ = (FlatDiscount, BulkDiscount);\n";

    #[test]
    fn an_error_in_another_contracts_file_is_located() {
        let e =
            outside_compile_error(E0423, &writes(&["flat_discount/src/lib.rs"])).expect("outside");
        assert_eq!(e.file, "pricing_engine/src/lib.rs");
        assert_eq!(e.symbol.as_deref(), Some("FlatDiscount"));
        assert!(e.message.starts_with("error[E0423]"), "{e:?}");
    }

    #[test]
    fn an_error_in_the_contracts_own_file_is_the_workers_to_fix() {
        assert_eq!(
            outside_compile_error(E0423, &writes(&["pricing_engine/src/lib.rs"])),
            None
        );
        let mixed =
            format!("{E0423}error[E0308]: mismatched types\n --> flat_discount/src/lib.rs:2:1\n");
        assert_eq!(
            outside_compile_error(&mixed, &writes(&["flat_discount/src/lib.rs"])),
            None,
            "one error of its own makes the round its own"
        );
    }

    #[test]
    fn a_panic_or_a_warning_is_not_a_compile_error() {
        let panic = "thread 'x' panicked at src/other.rs:12:5:\nnot yet implemented\n";
        assert_eq!(outside_compile_error(panic, &writes(&["a.rs"])), None);
        let warning = "warning: unused import: `x`\n --> src/other.rs:1:5\n";
        assert_eq!(outside_compile_error(warning, &writes(&["a.rs"])), None);
    }

    #[test]
    fn a_directory_write_covers_the_files_below_it() {
        let w = writes(&["crates/a/"]);
        assert_eq!(
            outside_compile_error("error: boom\n --> crates/a/src/lib.rs:1:1\n", &w),
            None
        );
    }

    #[test]
    fn the_amendment_quotes_the_error_and_names_the_file_and_symbol() {
        let e = outside_compile_error(E0423, &writes(&["x.rs"])).expect("outside");
        let text = amendment(&e);
        assert!(
            text.contains("expected value, found struct `FlatDiscount`"),
            "{text}"
        );
        assert!(
            text.contains(
                "The error is in `pricing_engine/src/lib.rs`, which uses `FlatDiscount` as the \
                 existing code defines it; keep the public shape that existing callers use."
            ),
            "{text}"
        );
    }
}
