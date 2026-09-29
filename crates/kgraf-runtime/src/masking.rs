//! Observation masking for workers (decision 0057, addendum 2).
//!
//! Workers never compact. They keep their private remainder small by **masking old
//! tool outputs continuously**, keeping the most recent few unmasked. This is not
//! compaction: nothing is paraphrased, and every pass runs over the original
//! transcript, never over a previous pass's output (0057: "a compacted context is
//! never compacted again").
//!
//! **The seam and its implementation.** The loop needs one call,
//! [`ObservationMasker::mask`]. [`SessionMasker`], the default, implements it over
//! `kgraf-session`'s reduction library (`reduction::reduce`, SWE-agent's
//! `LastNObservations` ported there), which replaced the in-crate stand-in this
//! module carried until the session library landed. Using the library rather than a
//! second copy of the algorithm keeps one masking rule for the whole system, and
//! the library's type rule (a `ReducedView` is not `Reducible`) is what makes
//! "never mask a masked transcript" a compile error rather than a convention.
//!
//! **Masking only, never truncation.** The library also truncates when masking
//! alone does not meet a token budget. A worker must not: it stops at its context
//! stop point and hands its remainder back (0057 addendum 2), and truncation would
//! drop messages, which this trait's contract forbids. The budget passed is
//! therefore unbounded, so the truncation pass never runs; the test
//! `nothing_is_ever_dropped_however_large_the_transcript` pins that.

use kgraf_provider::{Content, Message};
use kgraf_session::reduction::{MaskConfig, Reducible, TokenBudget, reduce};
use std::borrow::Cow;
use std::collections::BTreeSet;

/// Reduce a transcript before it is sent, without rewriting anything.
///
/// Contract: `mask` is a pure function of `messages`, which are always the
/// **original** transcript. It may replace the body of old tool results; it must
/// not change roles, the number or order of messages, tool-use ids, or any
/// non-tool-result block. The pinned instruction lives in the first user message
/// as text, so a masker that touches only tool results cannot drop it.
pub trait ObservationMasker: Send + Sync {
    /// The transcript as it will be sent.
    fn mask(&self, messages: &[Message]) -> Vec<Message>;
}

/// No masking. For tests and for a caller that has decided the transcript is small.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoMasking;

impl ObservationMasker for NoMasking {
    fn mask(&self, messages: &[Message]) -> Vec<Message> {
        messages.to_vec()
    }
}

/// SWE-agent's paper-cited window (0057 addendum 2).
pub const DEFAULT_KEEP_RECENT: usize = 5;

/// The worker's masker: `kgraf-session`'s `reduce`, masking only.
#[derive(Clone, Debug)]
pub struct SessionMasker {
    config: MaskConfig,
}

impl Default for SessionMasker {
    fn default() -> Self {
        Self::new(MaskConfig {
            keep_last_observations: DEFAULT_KEEP_RECENT,
            ..MaskConfig::default()
        })
    }
}

impl SessionMasker {
    /// Mask with `config`: keep the last `keep_last_observations` tool results,
    /// replace older ones with `placeholder` where that shortens them.
    pub fn new(config: MaskConfig) -> Self {
        Self { config }
    }

    /// The placeholder a masked tool result carries (with an `ERROR: ` prefix
    /// when the result was an error).
    pub fn placeholder(&self) -> &str {
        &self.config.placeholder
    }
}

/// One content block of the transcript, as a reduction entry. Borrowed unless
/// masked, so an unmasked block is cloned once, into the output, not per pass.
#[derive(Clone)]
struct Block<'a> {
    content: Cow<'a, Content>,
    /// The first message is the pinned instruction (0057).
    pinned: bool,
}

/// The local token estimate the library sizes with: four bytes per token, the
/// same rough rule the context estimate uses. Only the comparison "does masking
/// shrink this block" depends on it, and a byte count is monotone in either
/// direction.
fn approx(bytes: usize) -> usize {
    bytes.div_ceil(4)
}

impl Reducible for Block<'_> {
    fn approx_tokens(&self) -> usize {
        match self.content.as_ref() {
            Content::Text { text, .. } | Content::Reasoning { text } => approx(text.len()),
            Content::ToolResult { content, .. } => approx(content.len()),
            Content::ToolUse { name, input, .. } => approx(name.len() + input.to_string().len()),
        }
    }

    fn as_observation(&self) -> Option<&str> {
        match self.content.as_ref() {
            Content::ToolResult { content, .. } => Some(content),
            _ => None,
        }
    }

    fn masked(&self, placeholder: &str) -> Self {
        let content = match self.content.as_ref() {
            Content::ToolResult {
                tool_use_id,
                is_error,
                ..
            } => Content::ToolResult {
                tool_use_id: tool_use_id.clone(),
                // An error stays visibly an error, so the masked history still
                // shows which calls failed.
                content: if *is_error {
                    format!("ERROR: {placeholder}")
                } else {
                    placeholder.to_owned()
                },
                is_error: *is_error,
            },
            // `reduce` masks only entries whose `as_observation` is `Some`.
            other => other.clone(),
        };
        Self {
            content: Cow::Owned(content),
            pinned: self.pinned,
        }
    }

    fn is_pinned(&self) -> bool {
        self.pinned
    }
}

impl ObservationMasker for SessionMasker {
    fn mask(&self, messages: &[Message]) -> Vec<Message> {
        let blocks: Vec<Block<'_>> = messages
            .iter()
            .enumerate()
            .flat_map(|(i, m)| {
                m.content.iter().map(move |c| Block {
                    content: Cow::Borrowed(c),
                    pinned: i == 0,
                })
            })
            .collect();
        let view = reduce(
            &blocks,
            &BTreeSet::new(),
            // Unbounded: masking only, never truncation (see the module docs).
            TokenBudget(usize::MAX),
            &self.config,
        );
        if view.report.masked == 0 {
            return messages.to_vec();
        }
        let mut reduced = view.entries.into_iter();
        messages
            .iter()
            .map(|m| Message {
                role: m.role,
                content: m
                    .content
                    .iter()
                    .map(|original| {
                        reduced
                            .next()
                            .map(|r| r.item.content.into_owned())
                            // Unreachable while nothing is dropped; the original
                            // is the safe value if it ever were.
                            .unwrap_or_else(|| original.clone())
                    })
                    .collect(),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kgraf_provider::Role;

    fn result(id: usize, lines: usize) -> Content {
        Content::ToolResult {
            tool_use_id: format!("t{id}"),
            content: vec!["a line of tool output"; lines].join("\n"),
            is_error: false,
        }
    }

    fn transcript(results: usize, lines: usize) -> Vec<Message> {
        let mut messages = vec![Message::user("the instruction, verbatim")];
        for i in 0..results {
            messages.push(Message {
                role: Role::Assistant,
                content: vec![Content::ToolUse {
                    id: format!("t{i}"),
                    name: "read".into(),
                    input: serde_json::json!({}),
                }],
            });
            messages.push(Message {
                role: Role::User,
                content: vec![result(i, lines)],
            });
        }
        messages
    }

    fn bodies(messages: &[Message]) -> Vec<String> {
        messages
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|c| match c {
                Content::ToolResult { content, .. } => Some(content.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn all_but_the_last_five_tool_results_are_masked_and_nothing_else_changes() {
        let original = transcript(8, 3);
        let masker = SessionMasker::default();
        let masked = masker.mask(&original);
        assert_eq!(masked.len(), original.len());
        let b = bodies(&masked);
        assert!(b[..3].iter().all(|body| body == masker.placeholder()));
        assert!(b[3..].iter().all(|body| body.starts_with("a line")));
        // The instruction and every tool use survive untouched.
        assert_eq!(masked[0], original[0]);
        for (m, o) in masked.iter().zip(&original) {
            assert_eq!(m.role, o.role);
            if o.role == Role::Assistant {
                assert_eq!(m, o);
            }
        }
    }

    #[test]
    fn fewer_results_than_the_window_are_left_alone() {
        let original = transcript(5, 3);
        assert_eq!(SessionMasker::default().mask(&original), original);
    }

    #[test]
    fn a_result_shorter_than_the_placeholder_is_not_masked() {
        // Masking exists to save tokens; the library never grows an entry.
        let original = transcript(8, 0);
        assert_eq!(SessionMasker::default().mask(&original), original);
    }

    #[test]
    fn masking_is_a_function_of_the_original_so_a_second_pass_changes_nothing_more() {
        let original = transcript(9, 3);
        let m = SessionMasker::new(MaskConfig {
            keep_last_observations: 3,
            ..MaskConfig::default()
        });
        let once = m.mask(&original);
        assert_eq!(m.mask(&original), once);
    }

    #[test]
    fn an_error_result_stays_visibly_an_error_when_masked() {
        let long = "ERROR: not found, and a long explanation of why".to_owned();
        let messages = vec![
            Message::user("instruction"),
            Message {
                role: Role::User,
                content: vec![Content::ToolResult {
                    tool_use_id: "a".into(),
                    content: long,
                    is_error: true,
                }],
            },
            Message {
                role: Role::User,
                content: vec![result(1, 3)],
            },
        ];
        let m = SessionMasker::new(MaskConfig {
            keep_last_observations: 1,
            ..MaskConfig::default()
        });
        let masked = m.mask(&messages);
        match &masked[1].content[0] {
            Content::ToolResult {
                content, is_error, ..
            } => {
                assert!(*is_error);
                assert_eq!(content, &format!("ERROR: {}", m.placeholder()));
            }
            other => panic!("wrong block {other:?}"),
        }
    }

    #[test]
    fn the_pinned_instruction_is_never_masked_even_when_it_is_a_tool_result() {
        let mut messages = transcript(8, 3);
        messages[0] = Message {
            role: Role::User,
            content: vec![result(99, 10)],
        };
        let masked = SessionMasker::default().mask(&messages);
        assert_eq!(masked[0], messages[0]);
    }

    #[test]
    fn nothing_is_ever_dropped_however_large_the_transcript() {
        let original = transcript(200, 400);
        let masked = SessionMasker::default().mask(&original);
        assert_eq!(masked.len(), original.len());
        for (m, o) in masked.iter().zip(&original) {
            assert_eq!(m.content.len(), o.content.len());
        }
    }
}
