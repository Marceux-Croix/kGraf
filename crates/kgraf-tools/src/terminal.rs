//! Text shown at a terminal that a model or an outside party wrote.
//!
//! An approval question quotes the call it asks about. Printed raw, a model's
//! argument can carry escape sequences (clear the line, move the cursor,
//! recolor), a carriage return that overwrites what came before it, or
//! bidirectional overrides that reorder what the user reads, so the command
//! the user approves is not the command that runs. Joining argv with spaces
//! also hides where one argument ends: `["rm", "a b"]` and `["rm", "a", "b"]`
//! print the same.
//!
//! Written new: the escaping is a few lines over `char` classes, and the
//! shell-quoting crates searched (`shell-escape`, `shlex`) quote for a shell
//! to re-read, not for a person, and pass control characters through inside
//! quotes.

/// Whether `c` changes how a terminal shows the text around it, or shows as
/// blank space a reader cannot tell apart from an ordinary space: control
/// characters (U+0000 to U+001F, DEL and U+0080 to U+009F, which include ESC
/// and carriage return), every default-ignorable code point (bidirectional
/// controls, zero-width characters, variation selectors, tag characters, the
/// Hangul fillers; the list [`kgraf_core::paths::is_default_ignorable`] also
/// folds names with), the Unicode line and paragraph separators, the
/// typographic spaces, and the blank Braille pattern.
fn is_invisible_or_control(c: char) -> bool {
    c.is_control()
        || kgraf_core::paths::is_default_ignorable(c)
        || matches!(c,
            '\u{2000}'..='\u{200A}'
            | '\u{2028}'..='\u{202F}'
            | '\u{205F}'
            | '\u{2800}'
            | '\u{FFF9}'..='\u{FFFB}')
}

fn push_escaped(out: &mut String, c: char) {
    out.push_str(&format!("\\u{{{:x}}}", c as u32));
}

/// `text` with every control, bidirectional and invisible format character
/// written as a visible `\u{..}` escape. Line breaks are escaped too: the
/// text is shown on one prompt line.
pub fn terminal_safe(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if is_invisible_or_control(c) {
            push_escaped(&mut out, c);
        } else {
            out.push(c);
        }
    }
    out
}

/// Characters an argument may hold and still be shown bare.
fn is_plain(c: char) -> bool {
    c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c)
}

/// `cmd` shown for a person to approve: each argument separated by one space,
/// shown bare when it holds only plain characters and otherwise in single
/// quotes, with `\` and `'` inside escaped by a backslash and every control
/// or invisible character written as `\u{..}`. Two different argvs never
/// print the same.
pub fn quote_argv(cmd: &[String]) -> String {
    let mut out = String::new();
    for (i, arg) in cmd.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        if !arg.is_empty() && arg.chars().all(is_plain) {
            out.push_str(arg);
            continue;
        }
        out.push('\'');
        for c in arg.chars() {
            match c {
                '\\' | '\'' => {
                    out.push('\\');
                    out.push(c);
                }
                c if is_invisible_or_control(c) => push_escaped(&mut out, c),
                c => out.push(c),
            }
        }
        out.push('\'');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn escape_sequences_and_carriage_returns_are_made_visible() {
        let shown = terminal_safe("ls\r\u{1b}[2Kcargo test\u{202E}txt.exe\u{7}");
        assert!(!shown.chars().any(is_invisible_or_control), "{shown}");
        assert_eq!(shown, "ls\\u{d}\\u{1b}[2Kcargo test\\u{202e}txt.exe\\u{7}");
        assert_eq!(terminal_safe("plain text, kept"), "plain text, kept");
        assert_eq!(terminal_safe("caf\u{e9} \u{4e2d}"), "caf\u{e9} \u{4e2d}");
    }

    #[test]
    fn every_argument_boundary_is_visible() {
        assert_eq!(
            quote_argv(&argv(&["cargo", "test", "-p", "x"])),
            "cargo test -p x"
        );
        assert_ne!(
            quote_argv(&argv(&["rm", "a b"])),
            quote_argv(&argv(&["rm", "a", "b"]))
        );
        assert_eq!(quote_argv(&argv(&["rm", "a b"])), "rm 'a b'");
        assert_eq!(quote_argv(&argv(&["echo", ""])), "echo ''");
        assert_eq!(quote_argv(&argv(&["echo", "it's"])), "echo 'it\\'s'");
        assert_eq!(quote_argv(&argv(&["echo", "a\\' b"])), "echo 'a\\\\\\' b'");
        assert_ne!(
            quote_argv(&argv(&["echo", "a' 'b"])),
            quote_argv(&argv(&["echo", "a", "b"]))
        );
    }

    #[test]
    fn control_characters_inside_an_argument_are_escaped() {
        let shown = quote_argv(&argv(&["sh", "-c", "true\r\u{1b}[1Aecho safe"]));
        assert_eq!(shown, "sh -c 'true\\u{d}\\u{1b}[1Aecho safe'");
        assert!(!shown.chars().any(is_invisible_or_control));
        let bidi = quote_argv(&argv(&["cat", "a\u{202E}gnp.sh"]));
        assert_eq!(bidi, "cat 'a\\u{202e}gnp.sh'");
    }

    /// Characters that show as nothing or as a plain-looking space: tag
    /// characters (which can spell hidden text), Hangul and Khmer fillers,
    /// the blank Braille pattern, typographic spaces and variation selectors.
    #[test]
    fn blank_and_hidden_characters_are_escaped() {
        for c in [
            '\u{E0000}',
            '\u{E0041}',
            '\u{E007F}',
            '\u{3164}',
            '\u{115F}',
            '\u{1160}',
            '\u{17B4}',
            '\u{17B5}',
            '\u{2800}',
            '\u{2000}',
            '\u{2005}',
            '\u{200A}',
            '\u{202F}',
            '\u{205F}',
            '\u{FE00}',
            '\u{FE0F}',
            '\u{FFA0}',
            '\u{180E}',
        ] {
            let text = format!("rm{c}x");
            let shown = terminal_safe(&text);
            assert_eq!(shown, format!("rm\\u{{{:x}}}x", c as u32), "{:x}", c as u32);
            let quoted = quote_argv(&argv(&["echo", &text]));
            assert!(
                quoted.contains(&format!("\\u{{{:x}}}", c as u32)),
                "{quoted}"
            );
        }
        // Ordinary spaces and letters are shown as they are.
        assert_eq!(terminal_safe("a b\u{e9}"), "a b\u{e9}");
    }
}
