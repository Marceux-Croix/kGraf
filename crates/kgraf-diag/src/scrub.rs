//! Path scrubbing, secret-redaction handoff, and the context-size cap for
//! diagnostics text.
//!
//! **Decision: relative inside the project, keyed-hashed outside it.** A path
//! under the project root is already visible to anyone who has the
//! repository open (it names a file the user is about to share diagnostics
//! *about*), and it is far more useful for a diagnosis than an opaque token:
//! "cannot read `crates/kgraf-sandbox/src/error.rs`" tells the user exactly
//! where to look. A path outside the project root usually passes through the
//! user's home directory, which embeds the account name
//! (`/home/marco/...`, `C:\Users\marco\...`); that name is not diagnostic
//! information, so it is hashed instead of shown.
//!
//! **The hash is keyed (`key.rs`), not plain BLAKE3.** An unkeyed hash of a
//! path is reversible by guessing (a dictionary of common account and folder
//! names, hashed offline, matched against the diagnostics file). The key is a
//! random 32 bytes generated once per install and persisted next to the
//! diagnostics file, never inside it or inside an exported copy, so the hash
//! is stable **for the life of the install**, not merely "within one
//! process" (an earlier version of this doc claimed the latter; it was
//! wrong: the key was unkeyed-per-process BLAKE3 with no persisted key at
//! all, so two different runs did not even agree on the same hash for the
//! same path).
//!
//! **Secrets that are not paths.** A credentialed URL
//! (`https://user:token@host/...`), an API key embedded in a JSON error body,
//! or a registered exact credential can all reach an error's context string,
//! and none of those are paths this module's regex looks for.
//! [`redact_secrets`] hands the already-path-scrubbed text to
//! `kgraf_secrets::scrub_persist` (decision 0071): the *persist* path, not
//! the model-facing one, because this diagnostics file is written to disk and
//! read back later, never sent to a model.

use regex::Regex;
use std::path::Path;
use std::sync::OnceLock;

/// The context field's stored size cap: a
/// provider error's body or a stack of `anyhow` causes can run to kilobytes,
/// and none of that detail is needed once the code and the first line or two
/// of context are visible; it also bounds how much of a still-unredacted
/// secret (see the module doc) a truncated context can carry.
pub const MAX_CONTEXT_BYTES: usize = 512;

/// A path scrubbed for the diagnostics file: relative to `project_root` when
/// it is inside it, otherwise a keyed hash. Never panics: any path that does
/// not decode as UTF-8 is hashed too, since a lossy rendering could still
/// leak an account name.
pub fn scrub_path(path: &Path, project_root: Option<&Path>) -> String {
    if let Some(root) = project_root
        && let Ok(rel) = path.strip_prefix(root)
    {
        let rel = rel.to_string_lossy().replace('\\', "/");
        // The root itself (`rel` empty, e.g. `--root .` scrubbing its own
        // path) must not collapse to an empty string: that reads as "no
        // path" rather than "the project root".
        return if rel.is_empty() { ".".to_owned() } else { rel };
    }
    hash_path(path)
}

/// `h:` plus 16 hex characters of a keyed BLAKE3 digest.
fn hash_path(path: &Path) -> String {
    hash_path_with_key(path, &install_key())
}

/// [`hash_path`], with the key given explicitly rather than read from
/// [`install_key`]. Split out so a test can prove the hash is actually keyed
/// (two different keys over the same path must not collide): a mutation to
/// `hash_path` that silently drops the key argument and calls
/// `blake3::hash` instead still passes every other test in this file, since
/// none of them constructs the digest a second time under a key they chose
/// themselves to compare against.
fn hash_path_with_key(path: &Path, key: &[u8; crate::key::KEY_LEN]) -> String {
    let digest = blake3::keyed_hash(key, path.to_string_lossy().as_bytes());
    format!("h:{}", &digest.to_hex()[..16])
}

/// The per-install key (`key.rs`), loaded once per process. Falls back to a
/// random, process-local key when there is nowhere to persist one: no home
/// directory, or diagnostics is disabled or redirected
/// (`kgraf_diag::resolve_path`, not `default_path` -- review finding,
/// 2026-09-27: scrubbing used the real per-user path unconditionally, so a
/// process that set `KGRAF_DIAG_DISABLE` to keep a test off the owner's real
/// file still silently created `diagnostics.key` there the first time it
/// scrubbed any text). A fallback key is still keyed, just not stable across
/// runs on that machine, which is strictly better than the unkeyed hash this
/// replaced.
#[cfg(not(test))]
fn install_key() -> [u8; crate::key::KEY_LEN] {
    static KEY: OnceLock<[u8; crate::key::KEY_LEN]> = OnceLock::new();
    *KEY.get_or_init(|| {
        crate::paths::resolve_path()
            .and_then(|diag_path| {
                crate::key::load_or_create(&crate::key::key_path(&diag_path)).ok()
            })
            .unwrap_or_else(random_fallback_key)
    })
}

/// This crate's own unit tests never go through a binary's `main` (which is
/// the only place `KGRAF_DIAG_PATH`/`KGRAF_DIAG_DISABLE` get set in a real
/// run), so the real `install_key` above would fall through to
/// `resolve_path`'s default and silently touch the owner's actual per-user
/// key file the first time any test called `scrub_path`/`scrub_text` (this
/// is exactly how it was caught: a `diagnostics.key` kept appearing in
/// `~/Library/Application Support/kGraf` after `cargo test -p kgraf-diag`).
/// A fixed key here keeps every unit test both deterministic and, more to
/// the point for this crate specifically, off the real filesystem location
/// entirely. This only affects a `cfg(test)` build of *this* crate: a
/// dependent crate's tests, and the real `diag-smoke` binary the
/// cross-process integration tests spawn, both link the normal (non-`cfg
/// (test)`) build and get the real key above.
#[cfg(test)]
fn install_key() -> [u8; crate::key::KEY_LEN] {
    *b"kgraf-diag-test-key-32-bytes!!!!"
}

#[cfg(not(test))]
fn random_fallback_key() -> [u8; crate::key::KEY_LEN] {
    let mut key = [0u8; crate::key::KEY_LEN];
    // A syscall RNG failing here (vanishingly rare: it would mean the OS's
    // own randomness source is unavailable) still must not panic a
    // diagnostics call; an all-zero key just means paths hash unkeyed for
    // this process, no worse than before this fix.
    let _ = getrandom::fill(&mut key);
    key
}

fn path_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(concat!(
            // `file://` first (longest match wins nothing in the `regex`
            // crate's leftmost-first semantics, so more specific
            // alternatives are listed before the general ones they would
            // otherwise be swallowed by): captures the path after the
            // scheme so `scrub_path` sees a normal absolute path.
            r#"file://(?P<file_url>/[^\s"'`\[\]\(\)\{\}<>]*)"#,
            r#"|"#,
            // Windows drive path. Spaces are allowed inside it (a Windows
            // path routinely has one, e.g. "Marco Cross"); it still stops at
            // a quote, bracket or newline.
            r#"(?P<win>[A-Za-z]:[\\/][^"'`\[\]\{\}<>\r\n\t]+)"#,
            r#"|"#,
            // A bare Unix-style absolute path: stops at the first
            // whitespace, quote or bracket, since prose after it is not part
            // of the path.
            r#"(?P<unix>/[^\s"'`\[\]\(\)\{\}<>]+)"#,
        ))
        .expect("the path pattern is a fixed, tested literal")
    })
}

/// Scrub every path-looking span in free text (an error's `Display` chain,
/// typically): a regex match, not a parser, so it cannot tell a path from a
/// URL that happens to contain one (a `file://` URL is deliberately treated
/// as a path; an `https://` URL is not, and is [`redact_secrets`]'s job).
/// Also runs [`redact_secrets`] and caps the result at [`MAX_CONTEXT_BYTES`].
pub fn scrub_text(text: &str, project_root: Option<&Path>) -> String {
    let path_scrubbed = path_pattern().replace_all(text, |caps: &regex::Captures| {
        if let Some(m) = caps.name("file_url") {
            format!("file://{}", scrub_path(Path::new(m.as_str()), project_root))
        } else if let Some(m) = caps.name("win") {
            scrub_path(Path::new(m.as_str()), project_root)
        } else if let Some(m) = caps.name("unix") {
            scrub_path(Path::new(m.as_str()), project_root)
        } else {
            caps[0].to_string()
        }
    });
    let secret_scrubbed = redact_secrets(&path_scrubbed);
    truncate_bytes(&secret_scrubbed, MAX_CONTEXT_BYTES)
}

/// Redacts a registered exact credential (`kgraf_secrets::register_exact_secret`,
/// called wherever kGraf reads a user's own credential, for example a
/// provider API key from its environment variable) and a pattern-matched
/// secret (a GitHub PAT, a credentialed URL, and the rest of the rules ported
/// from gitleaks: `kgraf-secrets/src/rules.rs`), by way of
/// `kgraf_secrets::scrub_persist`, the *persist* path
/// (`kgraf_secrets::Scrubber::scrub_persist`): this diagnostics file is
/// written to disk and read back later by the user, never sent to a model, so
/// the persist path's marker (`[redacted:<kind>; re-read the source for exact
/// text]`) is the right one, not the model-facing path's.
fn redact_secrets(text: &str) -> String {
    kgraf_secrets::scrub_persist(text).into_owned()
}

/// Truncate `s` to at most `max_bytes`, on a `char` boundary, appending a
/// truncation marker (counted against the cap) when it cuts anything.
fn truncate_bytes(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_owned();
    }
    const MARK: &str = "\u{2026}[truncated]";
    let budget = max_bytes.saturating_sub(MARK.len());
    let mut end = budget.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{MARK}", &s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> std::path::PathBuf {
        std::path::PathBuf::from("/home/marco/kgraf")
    }

    #[test]
    fn inside_root_is_relative() {
        let p = Path::new("/home/marco/kgraf/crates/kgraf-diag/src/scrub.rs");
        assert_eq!(
            scrub_path(p, Some(&root())),
            "crates/kgraf-diag/src/scrub.rs"
        );
    }

    #[test]
    fn the_root_itself_is_a_dot_not_empty() {
        assert_eq!(scrub_path(&root(), Some(&root())), ".");
    }

    /// Regression for the keyed hash itself: mutating
    /// `hash_path` to call plain `blake3::hash` (dropping the key) passed
    /// every other test in this crate, since none of them compared two
    /// digests taken under two keys chosen by the test. This one does:
    /// two distinct 32-byte keys over the identical path must produce
    /// different digests. `hash_path_with_key` exists specifically so a test
    /// can supply the key explicitly instead of going through the shared,
    /// cached `install_key`.
    #[test]
    fn the_hash_is_actually_keyed_not_plain_blake3() {
        let p = Path::new("/home/marco/.ssh/id_ed25519");
        let key_a = [0xAAu8; crate::key::KEY_LEN];
        let key_b = [0xBBu8; crate::key::KEY_LEN];
        let a = hash_path_with_key(p, &key_a);
        let b = hash_path_with_key(p, &key_b);
        assert_ne!(
            a, b,
            "the same path under two different keys must hash differently; \
             a plain, unkeyed blake3::hash would give the same digest for both"
        );
        // Same key, same path: still deterministic.
        assert_eq!(hash_path_with_key(p, &key_a), a);
    }

    #[test]
    fn outside_root_is_hashed_and_stable_across_calls() {
        let p = Path::new("/home/marco/.ssh/id_ed25519");
        let a = scrub_path(p, Some(&root()));
        let b = scrub_path(p, Some(&root()));
        assert_eq!(a, b);
        assert!(a.starts_with("h:"));
        assert!(!a.contains("marco"));
    }

    #[test]
    fn no_root_hashes_absolute_paths() {
        let p = Path::new("/home/marco/project/file.rs");
        assert!(scrub_path(p, None).starts_with("h:"));
    }

    #[test]
    fn text_scrubs_a_backtick_quoted_path_and_keeps_the_rest() {
        let text = "cannot read `/home/marco/kgraf/Cargo.toml`: permission denied";
        let out = scrub_text(text, Some(&root()));
        assert!(out.contains("Cargo.toml"));
        assert!(!out.contains("/home/marco"));
        assert!(out.ends_with("permission denied"));
    }

    #[test]
    fn bracketed_path_keeps_its_brackets() {
        let out = scrub_text("cannot open [/home/marco/secret.txt]", None);
        assert!(out.starts_with("cannot open [h:"));
        assert!(out.ends_with(']'));
    }

    #[test]
    fn path_after_equals_is_found() {
        let out = scrub_text("path=/home/marco/secret.txt", None);
        assert!(out.starts_with("path=h:"));
    }

    #[test]
    fn file_url_keeps_its_scheme() {
        let out = scrub_text("file:///home/marco/secret.txt", None);
        assert!(out.starts_with("file://h:"), "{out}");
    }

    #[test]
    fn windows_path_with_a_space_is_scrubbed_whole() {
        let out = scrub_text(r"cannot read C:\Users\Marco Cross\secret.txt", None);
        assert!(out.starts_with("cannot read h:"), "{out}");
        assert!(!out.contains("Marco Cross"));
    }

    #[test]
    fn a_tab_before_a_path_does_not_stop_the_match() {
        let out = scrub_text("failed\t/home/marco/tab.txt", None);
        assert!(out.starts_with("failed\th:"));
    }

    #[test]
    fn a_multiline_leading_path_is_scrubbed() {
        let out = scrub_text("error:\n/home/marco/.ssh/id_ed25519 missing", None);
        assert!(out.contains("error:\nh:"), "{out}");
        assert!(out.ends_with("missing"));
    }

    #[test]
    fn a_json_embedded_path_is_scrubbed_and_the_json_survives() {
        let out = scrub_text(r#"{"path":"/home/marco/x"}"#, None);
        assert!(out.starts_with(r#"{"path":"h:"#));
        assert!(out.ends_with("\"}"));
    }

    #[test]
    fn relative_word_without_a_separator_is_untouched() {
        assert_eq!(scrub_text("Cargo.toml", None), "Cargo.toml");
    }

    /// A pattern-matched secret (a GitHub PAT: `kgraf-secrets/src/rules.rs`'s
    /// `ghp_[0-9a-zA-Z]{36}` rule) never reaches `scrub_text`'s output.
    #[test]
    fn a_pattern_matched_secret_is_redacted() {
        let token = format!("ghp_{}", "A".repeat(36));
        let out = scrub_text(&format!("git push failed: {token}"), None);
        assert!(!out.contains(&token), "{out}");
        assert!(out.contains("redacted"), "{out}");
    }

    /// A registered exact secret (kgraf-secrets' own registry, populated
    /// wherever kGraf reads a user's credential) never reaches `scrub_text`'s
    /// output either, end to end through the real diagnostics file: this is
    /// the actual guarantee ("no secrets... reach it"),
    /// not just a property of the in-memory string. Mutation-checked: see
    /// `scrub.rs`'s history for the pass-through this replaced, which this
    /// test (and `a_pattern_matched_secret_is_redacted` above) would have
    /// caught immediately.
    #[test]
    fn a_registered_key_and_a_pattern_secret_never_reach_diagnostics_jsonl() {
        let exact = "my-unique-fake-provider-key-9f3a7c21";
        kgraf_secrets::register_exact_secret("test-fixture-provider-key", exact);
        let ghp = format!("ghp_{}", "B".repeat(36));
        // Neither secret is inside anything the path regex would already
        // intercept first (a leading `/`): this isolates what
        // kgraf_secrets::scrub_persist itself catches from what scrub_path
        // already would.
        let raw_context = format!("provider call failed: key={exact} token={ghp}");
        let scrubbed = scrub_text(&raw_context, None);
        assert!(!scrubbed.contains(exact), "{scrubbed}");
        assert!(!scrubbed.contains(&ghp), "{scrubbed}");

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("diagnostics.jsonl");
        crate::store::append(
            &path,
            &crate::event::Record::now(crate::event::DiagEvent::error(
                "KG-0302",
                "kgraf-provider",
                scrubbed,
            )),
            crate::store::DEFAULT_CAP_BYTES,
        )
        .unwrap();

        let raw_file = std::fs::read_to_string(&path).unwrap();
        assert!(
            !raw_file.contains(exact),
            "the registered key reached diagnostics.jsonl: {raw_file}"
        );
        assert!(
            !raw_file.contains(&ghp),
            "the pattern-matched token reached diagnostics.jsonl: {raw_file}"
        );
        let records = crate::store::read_all(&path).unwrap();
        assert_eq!(records.len(), 1);
    }

    #[test]
    fn context_over_the_cap_is_truncated_on_a_char_boundary() {
        let long = "e".repeat(1000);
        let out = scrub_text(&long, None);
        assert!(out.len() <= MAX_CONTEXT_BYTES);
        assert!(out.ends_with("[truncated]"));
    }

    #[test]
    fn a_multibyte_string_near_the_cap_does_not_panic() {
        let long = "\u{1F600}".repeat(200); // 4-byte emoji, 800 bytes total
        let out = scrub_text(&long, None);
        assert!(out.len() <= MAX_CONTEXT_BYTES);
    }
}
