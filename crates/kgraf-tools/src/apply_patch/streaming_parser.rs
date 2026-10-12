// Copied from openai/codex codex-rs/apply-patch/src/streaming_parser.rs
// at e72da2b53805894878023d01949a25a082e0a5cb, Apache-2.0.
// Changes: `use` paths retargeted from `crate::parser` to
// `crate::apply_patch::parser` (this crate's module tree); the `environment_id`
// preamble is still parsed (so a patch carrying it is not rejected) but its
// accessor was dropped along with `ApplyPatchArgs::environment_id`, which
// kGraf's apply-patch tool never reads. A line that is not a hunk header gets an
// error naming its real fault: a blank line, or an Add File line without its
// `+`, where upstream called every such line "not a valid hunk header" (models
// read that as a header problem and resent the same blank line or bare
// content). Test coverage trimmed to the cases
// exercising behavior this crate keeps (no per-character streaming API is
// exposed here, but the parser's internal state machine is still exactly
// upstream's, so the streaming tests double as parser tests).

use std::path::PathBuf;

use crate::apply_patch::parser::ADD_FILE_MARKER;
use crate::apply_patch::parser::BEGIN_PATCH_MARKER;
use crate::apply_patch::parser::CHANGE_CONTEXT_MARKER;
use crate::apply_patch::parser::DELETE_FILE_MARKER;
use crate::apply_patch::parser::EMPTY_CHANGE_CONTEXT_MARKER;
use crate::apply_patch::parser::END_PATCH_MARKER;
use crate::apply_patch::parser::EOF_MARKER;
use crate::apply_patch::parser::Hunk;
use crate::apply_patch::parser::MOVE_TO_MARKER;
use crate::apply_patch::parser::ParseError;
use crate::apply_patch::parser::UPDATE_FILE_MARKER;
use crate::apply_patch::parser::UpdateFileChunk;

use Hunk::*;
use ParseError::*;

const ENVIRONMENT_ID_MARKER: &str = "*** Environment ID:";

const VALID_HUNK_HEADERS: &str = "Valid hunk headers: '*** Add File: {path}', '*** Delete File: {path}', '*** Update File: {path}'";

/// Why `trimmed` cannot stand where a hunk header is expected (outside any
/// file section, or after a Delete File header).
fn not_a_header(trimmed: &str) -> String {
    if trimmed.is_empty() {
        format!(
            "the line is blank where a hunk header was expected; remove it. {VALID_HUNK_HEADERS}"
        )
    } else {
        format!("'{trimmed}' is not a valid hunk header. {VALID_HUNK_HEADERS}")
    }
}

/// Why `trimmed` cannot stand in an Add File section: every line of the new
/// file starts with `+`, so a blank line or bare content is the usual fault,
/// not a malformed header.
fn not_an_added_line(trimmed: &str) -> String {
    if trimmed.is_empty() {
        format!(
            "the line is blank. In an Add File section every line of the new file starts with \
             '+', so a blank line of the file is written as a line holding only '+'. A line that \
             starts a new section must be a hunk header. {VALID_HUNK_HEADERS}"
        )
    } else {
        format!(
            "'{trimmed}' does not start with '+'. In an Add File section every line of the new \
             file starts with '+' (a blank line of the file is a line holding only '+'). A line \
             that starts a new section must be a hunk header. {VALID_HUNK_HEADERS}"
        )
    }
}

#[derive(Debug, Default, Clone)]
pub(crate) struct StreamingPatchParser {
    line_buffer: String,
    state: StreamingParserState,
    line_number: usize,
}

#[derive(Debug, Default, Clone)]
struct StreamingParserState {
    mode: StreamingParserMode,
    hunks: Vec<Hunk>,
    environment_id: Option<String>,
}

#[derive(Debug, Default, Clone, Copy)]
enum StreamingParserMode {
    #[default]
    NotStarted,
    StartedPatch,
    AddFile,
    DeleteFile,
    UpdateFile {
        hunk_line_number: usize,
    },
    EndedPatch,
}

impl StreamingPatchParser {
    fn ensure_update_hunk_is_not_empty(&self, line: &str) -> Result<(), ParseError> {
        if let Some(UpdateFile { path, chunks, .. }) = self.state.hunks.last() {
            if chunks.is_empty()
                && let StreamingParserMode::UpdateFile { hunk_line_number } = self.state.mode
            {
                return Err(InvalidHunkError {
                    message: format!("Update file hunk for path '{}' is empty", path.display()),
                    line_number: hunk_line_number,
                });
            }
            if chunks
                .last()
                .is_some_and(|chunk| chunk.old_lines.is_empty() && chunk.new_lines.is_empty())
            {
                if line == END_PATCH_MARKER {
                    return Err(InvalidHunkError {
                        message: "Update hunk does not contain any lines".to_string(),
                        line_number: self.line_number,
                    });
                }
                return Err(InvalidHunkError {
                    message: format!(
                        "Unexpected line found in update hunk: '{line}'. Every line should start with ' ' (context line), '+' (added line), or '-' (removed line)"
                    ),
                    line_number: self.line_number,
                });
            }
        }
        Ok(())
    }

    fn handle_hunk_headers_and_end_patch(&mut self, trimmed: &str) -> Result<bool, ParseError> {
        if matches!(self.state.mode, StreamingParserMode::StartedPatch)
            && let Some(environment_id) = trimmed.strip_prefix(ENVIRONMENT_ID_MARKER)
        {
            if self.state.environment_id.is_some() {
                return Err(InvalidPatchError(
                    "apply_patch environment_id cannot be specified more than once".to_string(),
                ));
            }
            let environment_id = environment_id.trim();
            if environment_id.is_empty() {
                return Err(InvalidPatchError(
                    "apply_patch environment_id cannot be empty".to_string(),
                ));
            }
            self.state.environment_id = Some(environment_id.to_string());
            return Ok(true);
        }
        if trimmed == END_PATCH_MARKER {
            self.ensure_update_hunk_is_not_empty(trimmed)?;
            self.state.mode = StreamingParserMode::EndedPatch;
            return Ok(true);
        }
        if let Some(path) = trimmed.strip_prefix(ADD_FILE_MARKER) {
            self.ensure_update_hunk_is_not_empty(trimmed)?;
            self.state.hunks.push(AddFile {
                path: PathBuf::from(path),
                contents: String::new(),
            });
            self.state.mode = StreamingParserMode::AddFile;
            return Ok(true);
        }
        if let Some(path) = trimmed.strip_prefix(DELETE_FILE_MARKER) {
            self.ensure_update_hunk_is_not_empty(trimmed)?;
            self.state.hunks.push(DeleteFile {
                path: PathBuf::from(path),
            });
            self.state.mode = StreamingParserMode::DeleteFile;
            return Ok(true);
        }
        if let Some(path) = trimmed.strip_prefix(UPDATE_FILE_MARKER) {
            self.ensure_update_hunk_is_not_empty(trimmed)?;
            self.state.hunks.push(UpdateFile {
                path: PathBuf::from(path),
                move_path: None,
                chunks: Vec::new(),
            });
            self.state.mode = StreamingParserMode::UpdateFile {
                hunk_line_number: self.line_number,
            };
            return Ok(true);
        }
        Ok(false)
    }

    pub(crate) fn push_delta(&mut self, delta: &str) -> Result<Vec<Hunk>, ParseError> {
        for ch in delta.chars() {
            if ch == '\n' {
                let mut line = std::mem::take(&mut self.line_buffer);
                line.truncate(line.strip_suffix('\r').map_or(line.len(), str::len));
                self.line_number += 1;
                self.process_line(&line)?;
            } else {
                self.line_buffer.push(ch);
            }
        }

        Ok(self.state.hunks.clone())
    }

    pub(crate) fn finish(&mut self) -> Result<Vec<Hunk>, ParseError> {
        if !self.line_buffer.is_empty() {
            let line = std::mem::take(&mut self.line_buffer);
            self.line_number += 1;
            if line.trim() == END_PATCH_MARKER {
                self.ensure_update_hunk_is_not_empty(line.trim())?;
                self.state.mode = StreamingParserMode::EndedPatch;
            } else {
                self.process_line(&line)?;
            }
        }

        if !matches!(self.state.mode, StreamingParserMode::EndedPatch) {
            return Err(InvalidPatchError(
                "The last line of the patch must be '*** End Patch'".to_string(),
            ));
        }

        Ok(self.state.hunks.clone())
    }

    fn process_line(&mut self, line: &str) -> Result<(), ParseError> {
        let trimmed = line.trim();
        match self.state.mode {
            StreamingParserMode::NotStarted => {
                if trimmed == BEGIN_PATCH_MARKER {
                    self.state.mode = StreamingParserMode::StartedPatch;
                    return Ok(());
                }
                Err(InvalidPatchError(
                    "The first line of the patch must be '*** Begin Patch'".to_string(),
                ))
            }
            StreamingParserMode::StartedPatch => {
                if self.handle_hunk_headers_and_end_patch(trimmed)? {
                    return Ok(());
                }
                Err(InvalidHunkError {
                    message: not_a_header(trimmed),
                    line_number: self.line_number,
                })
            }
            StreamingParserMode::AddFile => {
                if self.handle_hunk_headers_and_end_patch(trimmed)? {
                    return Ok(());
                }
                if let Some(line_to_add) = line.strip_prefix('+')
                    && let Some(AddFile { contents, .. }) = self.state.hunks.last_mut()
                {
                    contents.push_str(line_to_add);
                    contents.push('\n');
                    return Ok(());
                }
                Err(InvalidHunkError {
                    message: not_an_added_line(trimmed),
                    line_number: self.line_number,
                })
            }
            StreamingParserMode::DeleteFile => {
                if self.handle_hunk_headers_and_end_patch(trimmed)? {
                    return Ok(());
                }
                Err(InvalidHunkError {
                    message: not_a_header(trimmed),
                    line_number: self.line_number,
                })
            }
            StreamingParserMode::UpdateFile { hunk_line_number } => {
                let update_line = line.trim_end();
                if self.handle_hunk_headers_and_end_patch(update_line)? {
                    return Ok(());
                }

                if let Some(UpdateFile {
                    move_path, chunks, ..
                }) = self.state.hunks.last_mut()
                {
                    if chunks.last().is_some_and(|chunk| chunk.is_end_of_file) {
                        if update_line.is_empty() {
                            return Ok(());
                        }
                        if update_line != EMPTY_CHANGE_CONTEXT_MARKER
                            && !update_line.starts_with(CHANGE_CONTEXT_MARKER)
                        {
                            return Err(InvalidHunkError {
                                message: format!(
                                    "Expected update hunk to start with a @@ context marker, got: '{line}'"
                                ),
                                line_number: self.line_number,
                            });
                        }
                    }

                    if chunks.is_empty()
                        && move_path.is_none()
                        && let Some(move_to_path) = update_line.strip_prefix(MOVE_TO_MARKER)
                    {
                        *move_path = Some(PathBuf::from(move_to_path));
                        self.state.mode = StreamingParserMode::UpdateFile { hunk_line_number };
                        return Ok(());
                    }

                    if (update_line == EMPTY_CHANGE_CONTEXT_MARKER
                        || update_line.starts_with(CHANGE_CONTEXT_MARKER))
                        && chunks.last().is_some_and(|chunk| {
                            chunk.old_lines.is_empty() && chunk.new_lines.is_empty()
                        })
                    {
                        return Err(InvalidHunkError {
                            message: format!(
                                "Unexpected line found in update hunk: '{line}'. Every line should start with ' ' (context line), '+' (added line), or '-' (removed line)"
                            ),
                            line_number: self.line_number,
                        });
                    }

                    if update_line == EMPTY_CHANGE_CONTEXT_MARKER {
                        chunks.push(UpdateFileChunk::default());
                        self.state.mode = StreamingParserMode::UpdateFile { hunk_line_number };
                        return Ok(());
                    }

                    if let Some(change_context) = update_line.strip_prefix(CHANGE_CONTEXT_MARKER) {
                        chunks.push(UpdateFileChunk {
                            change_context: Some(change_context.to_string()),
                            ..UpdateFileChunk::default()
                        });
                        self.state.mode = StreamingParserMode::UpdateFile { hunk_line_number };
                        return Ok(());
                    }

                    if update_line == EOF_MARKER {
                        if chunks.last().is_some_and(|chunk| {
                            chunk.old_lines.is_empty() && chunk.new_lines.is_empty()
                        }) {
                            return Err(InvalidHunkError {
                                message: "Update hunk does not contain any lines".to_string(),
                                line_number: self.line_number,
                            });
                        }
                        if let Some(chunk) = chunks.last_mut() {
                            chunk.is_end_of_file = true;
                        }
                        self.state.mode = StreamingParserMode::UpdateFile { hunk_line_number };
                        return Ok(());
                    }

                    if line.is_empty() {
                        if chunks.is_empty() {
                            chunks.push(UpdateFileChunk::default());
                        }
                        if let Some(chunk) = chunks.last_mut() {
                            chunk.push_context_line(String::new());
                        }
                        self.state.mode = StreamingParserMode::UpdateFile { hunk_line_number };
                        return Ok(());
                    }

                    if let Some(line_to_add) = line.strip_prefix(' ') {
                        if chunks.is_empty() {
                            chunks.push(UpdateFileChunk::default());
                        }
                        if let Some(chunk) = chunks.last_mut() {
                            chunk.push_context_line(line_to_add.to_string());
                        }
                        self.state.mode = StreamingParserMode::UpdateFile { hunk_line_number };
                        return Ok(());
                    }

                    if let Some(line_to_add) = line.strip_prefix('+') {
                        if chunks.is_empty() {
                            chunks.push(UpdateFileChunk::default());
                        }
                        if let Some(chunk) = chunks.last_mut() {
                            chunk.new_lines.push(line_to_add.to_string());
                        }
                        self.state.mode = StreamingParserMode::UpdateFile { hunk_line_number };
                        return Ok(());
                    }

                    if let Some(line_to_remove) = line.strip_prefix('-') {
                        if chunks.is_empty() {
                            chunks.push(UpdateFileChunk::default());
                        }
                        if let Some(chunk) = chunks.last_mut() {
                            chunk.old_lines.push(line_to_remove.to_string());
                        }
                        self.state.mode = StreamingParserMode::UpdateFile { hunk_line_number };
                        return Ok(());
                    }

                    if chunks.last().is_some_and(|chunk| {
                        !chunk.old_lines.is_empty() || !chunk.new_lines.is_empty()
                    }) {
                        return Err(InvalidHunkError {
                            message: format!(
                                "Expected update hunk to start with a @@ context marker, got: '{line}'"
                            ),
                            line_number: self.line_number,
                        });
                    }
                }
                Err(InvalidHunkError {
                    message: format!(
                        "Unexpected line found in update hunk: '{line}'. Every line should start with ' ' (context line), '+' (added line), or '-' (removed line)"
                    ),
                    line_number: self.line_number,
                })
            }
            StreamingParserMode::EndedPatch => {
                if trimmed.is_empty() {
                    Ok(())
                } else {
                    Err(InvalidPatchError(
                        "The last line of the patch must be '*** End Patch'".to_string(),
                    ))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_streaming_patch_parser_streams_complete_lines_before_end_patch() {
        let mut parser = StreamingPatchParser::default();
        assert_eq!(
            parser.push_delta("*** Begin Patch\n*** Add File: src/hello.txt\n+hello\n+wor"),
            Ok(vec![AddFile {
                path: PathBuf::from("src/hello.txt"),
                contents: "hello\n".to_string(),
            }])
        );
        assert_eq!(
            parser.push_delta("ld\n"),
            Ok(vec![AddFile {
                path: PathBuf::from("src/hello.txt"),
                contents: "hello\nworld\n".to_string(),
            }])
        );
    }

    #[test]
    fn test_streaming_patch_parser_move_and_delete() {
        let mut parser = StreamingPatchParser::default();
        assert_eq!(
            parser.push_delta(
                "*** Begin Patch\n*** Update File: src/old.rs\n*** Move to: src/new.rs\n@@\n-old\n+new\n",
            ),
            Ok(vec![UpdateFile {
                path: PathBuf::from("src/old.rs"),
                move_path: Some(PathBuf::from("src/new.rs")),
                chunks: vec![UpdateFileChunk {
                    change_context: None,
                    old_lines: vec!["old".to_string()],
                    new_lines: vec!["new".to_string()],
                    context_line_indices: vec![],
                    is_end_of_file: false,
                    final_newline: None,
                }],
            }])
        );

        let mut parser = StreamingPatchParser::default();
        assert_eq!(
            parser.push_delta("*** Begin Patch\n*** Delete File: gone.txt"),
            Ok(Vec::new())
        );
        assert_eq!(
            parser.push_delta("\n"),
            Ok(vec![DeleteFile {
                path: PathBuf::from("gone.txt"),
            }])
        );
    }

    #[test]
    fn test_streaming_patch_parser_finish_processes_final_line_without_newline() {
        let mut parser = StreamingPatchParser::default();
        assert_eq!(
            parser.push_delta("*** Begin Patch\n*** Add File: file.txt\n+hello\n*** End Patch"),
            Ok(vec![AddFile {
                path: PathBuf::from("file.txt"),
                contents: "hello\n".to_string(),
            }])
        );
        assert_eq!(
            parser.finish(),
            Ok(vec![AddFile {
                path: PathBuf::from("file.txt"),
                contents: "hello\n".to_string(),
            }])
        );
    }

    #[test]
    fn test_streaming_patch_parser_finish_requires_end_patch() {
        let mut parser = StreamingPatchParser::default();
        assert_eq!(
            parser.push_delta("*** Begin Patch\n*** Add File: file.txt\n+hello\n"),
            Ok(vec![AddFile {
                path: PathBuf::from("file.txt"),
                contents: "hello\n".to_string(),
            }])
        );
        assert_eq!(
            parser.finish(),
            Err(InvalidPatchError(
                "The last line of the patch must be '*** End Patch'".to_string(),
            ))
        );
    }

    #[test]
    fn test_streaming_patch_parser_returns_errors() {
        let mut parser = StreamingPatchParser::default();
        assert_eq!(
            parser.push_delta("bad\n"),
            Err(InvalidPatchError(
                "The first line of the patch must be '*** Begin Patch'".to_string(),
            ))
        );

        let mut parser = StreamingPatchParser::default();
        assert_eq!(
            parser.push_delta("*** Begin Patch\n*** Update File: file.txt\n*** End Patch\n"),
            Err(InvalidHunkError {
                message: "Update file hunk for path 'file.txt' is empty".to_string(),
                line_number: 2,
            })
        );
    }

    /// A blank line in an Add File section is named as blank, with the way to
    /// write one, not called an invalid hunk header.
    #[test]
    fn a_blank_line_in_an_add_file_is_named_with_how_to_write_one() {
        let mut parser = StreamingPatchParser::default();
        let err = parser
            .push_delta("*** Begin Patch\n*** Add File: a.rs\n+fn a() {}\n\n+fn b() {}\n")
            .expect_err("a blank line is refused");
        let InvalidHunkError {
            message,
            line_number,
        } = err
        else {
            panic!("{err:?}")
        };
        assert_eq!(line_number, 4);
        assert!(message.starts_with("the line is blank."), "{message}");
        assert!(message.contains("a line holding only '+'"), "{message}");
        assert!(!message.contains("is not a valid hunk header"), "{message}");

        // The same blank line written as `+` is accepted as an empty line.
        let mut parser = StreamingPatchParser::default();
        let hunks = parser
            .push_delta(
                "*** Begin Patch\n*** Add File: a.rs\n+fn a() {}\n+\n+fn b() {}\n*** End Patch\n",
            )
            .expect("a `+` line");
        assert_eq!(
            hunks,
            vec![AddFile {
                path: PathBuf::from("a.rs"),
                contents: "fn a() {}\n\nfn b() {}\n".to_owned(),
            }]
        );
    }

    /// Bare content in an Add File section is named as missing its `+`.
    #[test]
    fn a_bare_line_in_an_add_file_is_named_as_missing_its_plus() {
        let mut parser = StreamingPatchParser::default();
        let err = parser
            .push_delta("*** Begin Patch\n*** Add File: Cargo.toml\n[package]\n")
            .expect_err("bare content is refused");
        let InvalidHunkError { message, .. } = err else {
            panic!("{err:?}")
        };
        assert!(
            message.starts_with("'[package]' does not start with '+'."),
            "{message}"
        );
        assert!(message.contains("Valid hunk headers"), "{message}");
    }

    /// A blank line where a hunk header is expected says so.
    #[test]
    fn a_blank_line_where_a_header_is_expected_is_named_as_blank() {
        for patch in [
            "*** Begin Patch\n\n*** Add File: a.rs\n",
            "*** Begin Patch\n*** Delete File: a.rs\n\n",
        ] {
            let mut parser = StreamingPatchParser::default();
            let err = parser
                .push_delta(patch)
                .expect_err("a blank line is refused");
            let InvalidHunkError { message, .. } = err else {
                panic!("{err:?}")
            };
            assert!(
                message.starts_with("the line is blank where a hunk header was expected"),
                "{patch:?}: {message}"
            );
        }
    }
}
