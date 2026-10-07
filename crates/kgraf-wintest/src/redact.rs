//! What the report must not carry: the user name, the computer name, the
//! build machine's source path, and anything shaped like a secret.
//!
//! Applied once to the whole rendered report (the Markdown and the embedded
//! JSON together), so no section can bypass it. A name is replaced only as a
//! whole token: `ann` in `C:\Users\ann\x` goes, `ann` in `channel` stays.

/// The names to replace and what replaces them.
#[derive(Clone, Debug, Default)]
pub struct Redactor {
    pairs: Vec<(String, String)>,
}

impl Redactor {
    /// `user` and `computer` are the Windows `USERNAME` and `COMPUTERNAME`;
    /// `build_root` is the source path the binaries were built from.
    pub fn new(user: Option<&str>, computer: Option<&str>, build_root: Option<&str>) -> Self {
        let mut pairs = Vec::new();
        if let Some(root) = build_root.filter(|r| r.len() > 3) {
            pairs.push((root.to_owned(), "<build-root>".to_owned()));
            // The same path as a Windows test binary sees it.
            pairs.push((root.replace('/', "\\"), "<build-root>".to_owned()));
            pairs.push((root.replace('/', "\\\\"), "<build-root>".to_owned()));
        }
        // Two characters or more: a one-letter name would match everywhere.
        if let Some(u) = user.map(str::trim).filter(|u| u.chars().count() >= 2) {
            pairs.push((u.to_owned(), "<user>".to_owned()));
        }
        if let Some(c) = computer.map(str::trim).filter(|c| c.chars().count() >= 2) {
            pairs.push((c.to_owned(), "<computer>".to_owned()));
        }
        Self { pairs }
    }

    /// Every name replaced (paths first, then whole-token names, ignoring
    /// case), then every secret pattern kGraf's own scrubber knows.
    pub fn apply(&self, text: &str) -> String {
        let mut out = text.to_owned();
        for (from, to) in &self.pairs {
            out = if from.contains(['/', '\\']) {
                out.replace(from.as_str(), to)
            } else {
                replace_token_ignore_case(&out, from, to)
            };
        }
        kgraf_secrets::Scrubber::new()
            .scrub_persist(&out)
            .into_owned()
    }
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Replace `needle` wherever it stands as a whole token, ignoring ASCII case.
pub fn replace_token_ignore_case(text: &str, needle: &str, with: &str) -> String {
    if needle.is_empty() {
        return text.to_owned();
    }
    let lower_text = text.to_lowercase();
    let lower_needle = needle.to_lowercase();
    // Lowercasing can change byte lengths outside ASCII; then only exact
    // matches are replaced, which still covers the usual case.
    if lower_text.len() != text.len() || lower_needle.len() != needle.len() {
        return replace_token_exact(text, needle, with);
    }
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while let Some(pos) = lower_text[i..].find(&lower_needle) {
        let start = i + pos;
        let end = start + needle.len();
        let before = text[..start].chars().next_back();
        let after = text[end..].chars().next();
        out.push_str(&text[i..start]);
        if before.is_none_or(|c| !is_word(c)) && after.is_none_or(|c| !is_word(c)) {
            out.push_str(with);
        } else {
            out.push_str(&text[start..end]);
        }
        i = end;
    }
    out.push_str(&text[i..]);
    out
}

fn replace_token_exact(text: &str, needle: &str, with: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while let Some(pos) = text[i..].find(needle) {
        let start = i + pos;
        let end = start + needle.len();
        let before = text[..start].chars().next_back();
        let after = text[end..].chars().next();
        out.push_str(&text[i..start]);
        if before.is_none_or(|c| !is_word(c)) && after.is_none_or(|c| !is_word(c)) {
            out.push_str(with);
        } else {
            out.push_str(&text[start..end]);
        }
        i = end;
    }
    out.push_str(&text[i..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_user_name_goes_from_paths_in_every_spelling() {
        let r = Redactor::new(Some("Ann"), Some("DESK-01"), None);
        let text = r#"C:\Users\ann\AppData and "C:\\Users\\Ann\\x" on DESK-01; channel Annex"#;
        let out = r.apply(text);
        assert_eq!(
            out,
            r#"C:\Users\<user>\AppData and "C:\\Users\\<user>\\x" on <computer>; channel Annex"#
        );
    }

    #[test]
    fn the_build_root_is_replaced_in_both_slash_forms() {
        let r = Redactor::new(None, None, Some("/home/dev/src/kgraf"));
        let out = r.apply(r"/home/dev/src/kgraf/crates/a and X:\home\dev\src\kgraf\crates\b");
        assert_eq!(out, r"<build-root>/crates/a and X:<build-root>\crates\b");
    }

    #[test]
    fn a_secret_shaped_value_is_scrubbed() {
        let r = Redactor::new(None, None, None);
        let key = format!("sk-ant-api03-{}", "a".repeat(93) + "AA");
        let out = r.apply(&format!("ANTHROPIC_API_KEY={key}"));
        assert!(!out.contains(&key), "{out}");
    }

    #[test]
    fn a_one_letter_name_is_not_used() {
        let r = Redactor::new(Some("a"), None, None);
        assert_eq!(r.apply("a b a"), "a b a");
    }
}
