//! The command line the Windows backend gives `CreateProcessAsUserW`.
//!
//! Pure text work, built on every platform: the command policy checks a
//! `cmd /c` or `powershell -Command` script against the text the
//! interpreter will read from this line (decision 0094), and that check is
//! tested on every development host, not only on Windows.

/// Append one argument to a UTF-16 command line under the MSVC runtime
/// parsing rules (the rules `CommandLineToArgvW` and Rust's std follow).
/// Quotes are added only around an empty argument or one with a space or a
/// tab.
pub(crate) fn append_arg(out: &mut Vec<u16>, arg: &[u16]) {
    const QUOTE: u16 = b'"' as u16;
    const SLASH: u16 = b'\\' as u16;
    if !out.is_empty() {
        out.push(u16::from(b' '));
    }
    let quote = arg.is_empty()
        || arg
            .iter()
            .any(|c| *c == u16::from(b' ') || *c == u16::from(b'\t'));
    if quote {
        out.push(QUOTE);
    }
    let mut slashes = 0usize;
    for &c in arg {
        if c == SLASH {
            slashes += 1;
        } else {
            if c == QUOTE {
                // Double the run of backslashes before a quote, then escape it.
                out.extend(std::iter::repeat_n(SLASH, slashes + 1));
            }
            slashes = 0;
        }
        out.push(c);
    }
    if quote {
        // Backslashes before the closing quote must be doubled.
        out.extend(std::iter::repeat_n(SLASH, slashes));
        out.push(QUOTE);
    }
}

/// The whole command line: the program always quoted and never escaped (as
/// Rust's std writes it; the backend refuses a program path holding a
/// quote), then each argument through [`append_arg`]. No terminating NUL.
pub(crate) fn command_line<I>(program: &[u16], args: I) -> Vec<u16>
where
    I: IntoIterator,
    I::Item: AsRef<[u16]>,
{
    let mut line: Vec<u16> = Vec::new();
    line.push(u16::from(b'"'));
    line.extend_from_slice(program);
    line.push(u16::from(b'"'));
    for arg in args {
        append_arg(&mut line, arg.as_ref());
    }
    line
}

/// [`command_line`] for UTF-8 text: the exact line the Windows backend
/// starts `program` with for `args`.
pub fn windows_command_line(program: &str, args: &[String]) -> String {
    let wide: Vec<Vec<u16>> = args.iter().map(|a| a.encode_utf16().collect()).collect();
    let program: Vec<u16> = program.encode_utf16().collect();
    String::from_utf16_lossy(&command_line(&program, &wide))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_program_is_quoted_and_arguments_follow_msvc_quoting() {
        let args: Vec<String> = ["/c", "dir & echo a b", r"C:\my dir\"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        assert_eq!(
            windows_command_line("cmd.exe", &args),
            r#""cmd.exe" /c "dir & echo a b" "C:\my dir\\""#
        );
    }
}
