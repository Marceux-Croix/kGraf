//! A patch the model wrote as text instead of calling `apply_patch`.
//!
//! Some models end a turn with a complete `*** Begin Patch ... *** End Patch`
//! block in their reply and no tool call. Left alone, the turn reads as a
//! finished attempt (or, under the no-change guard, as an announcement that
//! costs a notice round), and the patch is never applied. When the reply holds
//! exactly one complete block, the worker loop turns it into an `apply_patch`
//! call, so it passes the same checks, approval chain, ledger and trace as a
//! call the model made.
//!
//! This module only finds the block. It does not read the patch: the tool
//! parses and applies it, and a block the tool rejects is rejected as any
//! other call would be.

/// The line that opens a patch.
const BEGIN_MARKER: &str = "*** Begin Patch";

/// The line that closes a patch.
const END_MARKER: &str = "*** End Patch";

/// The one complete patch block in `text`, from its begin line to its end
/// line, as the `patch` value of an `apply_patch` call. `None` when the text
/// holds no block, more than one, or a block with no end line. A marker is a
/// whole line (surrounding space aside), so a patch line that quotes one
/// (`+*** End Patch`) does not close the block. Text around the block, such
/// as a code fence or a sentence, is left out.
pub(crate) fn single_block(text: &str) -> Option<String> {
    let mut found: Option<String> = None;
    let mut open: Option<Vec<&str>> = None;
    for line in text.lines() {
        let marker = line.trim();
        match open.as_mut() {
            None if marker == BEGIN_MARKER => open = Some(vec![BEGIN_MARKER]),
            None => {}
            // A second begin before the end: the first block is unfinished,
            // and which one the model meant is not known.
            Some(_) if marker == BEGIN_MARKER => return None,
            Some(lines) if marker == END_MARKER => {
                lines.push(END_MARKER);
                if found.is_some() {
                    return None;
                }
                found = Some(lines.join("\n"));
                open = None;
            }
            Some(lines) => lines.push(line),
        }
    }
    // A block still open at the end of the text has no end line.
    if open.is_some() {
        return None;
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLOCK: &str = "*** Begin Patch\n*** Update File: a.rs\n@@\n-old\n+new\n*** End Patch";

    #[test]
    fn one_block_is_found_with_the_text_around_it_left_out() {
        let text = format!("Here is the change:\n\n```\n{BLOCK}\n```\nDone.");
        assert_eq!(single_block(&text).as_deref(), Some(BLOCK));
    }

    #[test]
    fn no_block_two_blocks_or_an_unfinished_block_are_none() {
        assert_eq!(single_block("I will edit a.rs now."), None);
        assert_eq!(single_block(&format!("{BLOCK}\n\nand\n\n{BLOCK}")), None);
        assert_eq!(
            single_block("*** Begin Patch\n*** Update File: a.rs\n@@\n-old\n+new"),
            None
        );
        // A complete block, then a begin that never ends.
        assert_eq!(single_block(&format!("{BLOCK}\n*** Begin Patch\nx")), None);
        // A begin inside an open block.
        assert_eq!(
            single_block("*** Begin Patch\n*** Begin Patch\n*** End Patch"),
            None
        );
        // An end line with no begin.
        assert_eq!(single_block("*** End Patch"), None);
    }

    #[test]
    fn a_patch_line_that_quotes_a_marker_does_not_close_the_block() {
        let text = "*** Begin Patch\n*** Add File: notes.md\n+*** End Patch\n+text\n*** End Patch";
        assert_eq!(single_block(text).as_deref(), Some(text));
    }
}
