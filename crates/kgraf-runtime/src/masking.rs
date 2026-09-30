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

    /// How many of the most recent tool rounds keep their results unmasked,
    /// when the masker hides older ones. `None`: results are never hidden. The
    /// stall detector reads it: a call repeated after its result was hidden is
    /// a re-read, not a loop.
    fn observation_window(&self) -> Option<usize> {
        None
    }

    /// Whether a result with this body is one the masker replaces once it
    /// leaves the window. A result the placeholder would not shorten is never
    /// hidden, so a repeated call that produced it is a loop, not a re-read.
    /// Default `false`: a masker that does not say is assumed to hide nothing.
    fn hides_result(&self, _output: &str, _is_error: bool) -> bool {
        false
    }
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
    /// Mask with `config`: keep the tool results of the last
    /// `keep_last_observations` rounds (user messages that carry results),
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
    fn observation_window(&self) -> Option<usize> {
        Some(self.config.keep_last_observations)
    }

    fn hides_result(&self, output: &str, is_error: bool) -> bool {
        // The library's rule (`reduce`): masked only where the placeholder
        // costs fewer tokens than the result. An error keeps an `ERROR: `
        // prefix on its placeholder (`Block::masked`).
        let prefix = if is_error { "ERROR: ".len() } else { 0 };
        approx(prefix + self.config.placeholder.len()) < approx(output.len())
    }

    fn mask(&self, messages: &[Message]) -> Vec<Message> {
        // The window counts rounds, not blocks. One round is one user message
        // holding the results of every call the model issued together, and the
        // model has not seen those results yet when the round is sent. Counting
        // blocks would mask the first results of a wide round before the model
        // read them. Every block of the last `keep_last_observations` user
        // messages that carry tool results is pinned, and the library's own
        // block window is set to 0 so it masks the rest.
        //
        // Prompt cache: the window slides one round at a time, so a round's
        // results are masked as it leaves the window. That changes the bytes of
        // a message earlier in the request, once per round, the same as the
        // block window did.
        let keep_rounds = self.config.keep_last_observations;
        let recent: BTreeSet<usize> = messages
            .iter()
            .enumerate()
            .filter(|(_, m)| {
                m.content
                    .iter()
                    .any(|c| matches!(c, Content::ToolResult { .. }))
            })
            .map(|(i, _)| i)
            .rev()
            .take(keep_rounds)
            .collect();
        let config = MaskConfig {
            keep_last_observations: 0,
            ..self.config.clone()
        };
        let blocks: Vec<Block<'_>> = messages
            .iter()
            .enumerate()
            .flat_map(|(i, m)| {
                let pinned = i == 0 || recent.contains(&i);
                m.content.iter().map(move |c| Block {
                    content: Cow::Borrowed(c),
                    pinned,
                })
            })
            .collect();
        let view = reduce(
            &blocks,
            &BTreeSet::new(),
            // Unbounded: masking only, never truncation (see the module docs).
            TokenBudget(usize::MAX),
            &config,
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
                    signature: None,
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
    fn a_wide_round_is_kept_whole_and_the_window_counts_rounds() {
        // Eight results in one user message, then four single-result rounds:
        // five rounds in all, so nothing is masked. A block count would have
        // masked the first seven of the wide round.
        let mut messages = vec![Message::user("the instruction, verbatim")];
        messages.push(Message {
            role: Role::Assistant,
            content: (0..8)
                .map(|i| Content::ToolUse {
                    id: format!("w{i}"),
                    name: "read".into(),
                    input: serde_json::json!({}),
                    signature: None,
                })
                .collect(),
        });
        messages.push(Message {
            role: Role::User,
            content: (0..8).map(|i| result(100 + i, 3)).collect(),
        });
        messages.extend(transcript(4, 3).into_iter().skip(1));
        let masker = SessionMasker::default();
        assert_eq!(masker.mask(&messages), messages);
        // One more round pushes the wide round out of the window: all eight
        // of its blocks are masked together.
        messages.extend(transcript(1, 3).into_iter().skip(1));
        let masked = masker.mask(&messages);
        let b = bodies(&masked);
        assert!(b[..8].iter().all(|body| body == masker.placeholder()));
        assert!(b[8..].iter().all(|body| body.starts_with("a line")));
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
    #[test]
    fn a_result_is_hidden_only_where_the_placeholder_is_shorter() {
        let masker = SessionMasker::default();
        assert!(
            !masker.hides_result("ok", false),
            "a short result is never masked"
        );
        assert!(masker.hides_result(&"line of output\n".repeat(20), false));
        // The masker agrees with `mask` itself.
        let short = Message {
            role: Role::User,
            content: vec![Content::ToolResult {
                tool_use_id: "s".into(),
                content: "ok".into(),
                is_error: false,
            }],
        };
        let mut messages = vec![Message::user("go"), short.clone()];
        for i in 0..DEFAULT_KEEP_RECENT + 1 {
            messages.push(Message {
                role: Role::User,
                content: vec![Content::ToolResult {
                    tool_use_id: format!("t{i}"),
                    content: "ok".into(),
                    is_error: false,
                }],
            });
        }
        assert_eq!(
            masker.mask(&messages),
            messages,
            "short results stay visible"
        );
        // An error keeps a longer placeholder, so a mid-size error is not hidden.
        let mid = "x".repeat(masker.placeholder().len() + 8);
        assert!(masker.hides_result(&mid, false));
        assert!(!masker.hides_result(&mid, true));
    }
}
