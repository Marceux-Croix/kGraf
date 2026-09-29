//! Secret detection and redaction for anything kGraf writes to disk or shows in
//! the shell: worker transcripts (`.kgraf/runs/<run>/`, `kgraf-session::transcript`),
//! run traces, agent-to-agent messages, and the ledger (decision 0071).
//!
//! **Two paths, not one.** An earlier version of
//! this crate scrubbed pattern rules into everything, including the text a
//! worker's model is shown. That corrupted real code: `generic-api-key`,
//! ported without gitleaks' own entropy floor and allowlist, matched
//! `stats.code_tokens = code_spent;` in 50 of this repository's own 651
//! tracked files. A `read` tool's output feeding `apply_patch` context, or a
//! full-file rewrite, would then write `[redacted:generic-api-key]` to disk in
//! place of real source. There are now two independent scrub paths:
//!
//! - **Model-facing** ([`Scrubber::scrub_model_facing`]): the text a tool's
//!   output or input puts in front of the model (`WorkerEvent::ToolCallFinished`'s
//!   `output`, `ToolCallStarted`'s `input`). Redacts **only** the exact-secret
//!   registry: credentials kGraf itself holds, known exactly, never guessed at
//!   by shape. Pattern rules never run here, so precision is total: nothing a
//!   worker reads or writes can be corrupted by a rule that thinks ordinary
//!   code looks like a key. The model never needs kGraf's own credentials
//!   anyway (it never sends them; kGraf's provider and MCP clients do), so
//!   losing nothing by redacting them is exact, not a trade-off. A model
//!   reading a file that itself holds some *other* secret still sees it: that
//!   is what a `read` tool is for, and is inherent to giving a worker file
//!   access at all, not a gap in this crate.
//! - **Persist and display** ([`Scrubber::scrub_persist`]): everything written
//!   to the transcript, a trace, a run record, agent-to-agent comms, or shown
//!   in the shell. Redacts the registry, *plus* the pattern rules (a curated
//!   subset of gitleaks's default rule set, MIT; `third_party/gitleaks/UPSTREAM`),
//!   for a secret a tool printed or a model echoed that kGraf never configured
//!   itself. `generic-api-key` -- the rule responsible for the corruption above --
//!   is additionally gated the way gitleaks gates it: a Shannon-entropy floor
//!   ([`entropy::shannon_entropy`]) and gitleaks' own allowlist and stopword
//!   list for that rule ([`generic_api_key_allowlist`]), both ported with
//!   attribution.
//!
//! **A redaction on the persist path is self-describing.** The marker is
//! `[redacted:<kind>; re-read the source for exact text]`, not a bare
//! `[redacted:<kind>]`: a resumed worker rebuilds its context from the
//! *persisted*, pattern-scrubbed transcript (not from what the live run
//! actually showed the model, which was only registry-scrubbed), so it can see
//! a marker where the original run saw real text. The marker says so, and
//! names the live remedy: read the file, run the command, or otherwise re-derive
//! the value, rather than treat the marker as the file's actual content.
//!
//! **The exact-secret registry** ([`register_exact_secret`]) backs both paths:
//! a provider API key, an MCP header's environment value. These are found by
//! plain substring search, the same technique `kgraf-mcp`'s `Redactor` already
//! uses for MCP replies (`crates/kgraf-mcp/src/http.rs`) -- this registry is
//! the same idea, shared process-wide so kGraf's own credentials are scrubbed
//! everywhere, not only from what an MCP server echoes back.
//!
//! **Two copies of stored model input** ([`intake`], [`model_copy`]; decision
//! 0071). Text that is both model input and stored (a user's
//! instruction or note, a contract brief, a mailbox body) is stored
//! persist-scrubbed, and its registry-only copy is kept in process memory for
//! the model. After a restart the model reads the stored copy, markers and all.
//!
//! **Counts** ([`Scrubber::scrub_persist_counted`], [`Redactions`]): kinds and
//! counts of what a scrub redacted, never the text, so a caller can record
//! that a redaction happened.
//!
//! **Performance.** [`Scrubber::scrub_model_facing`] and
//! [`Scrubber::scrub_persist`] both run on every tool result. The common case
//! -- no secret present -- is a linear scan: the registry check is a handful
//! of `str::find` calls (there are at most a few dozen configured credentials,
//! never thousands), and the persist path's pattern check is a
//! [`regex::RegexSet`] built once, at [`Scrubber::new`], covering all pattern
//! rules in a single pass. See `benches/scrub.rs` for the 1&nbsp;MB measurement.

pub mod entropy;
pub mod generic_api_key_allowlist;
pub mod rules;

use regex::{Regex, RegexSet, RegexSetBuilder};
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, RwLock};

/// A `_`/`-`/`.`/space-separated word of a variable name that, matched whole
/// (case-insensitive) against one of these, marks the name as holding a
/// credential. See [`name_is_credential_shaped`], the one place this list is
/// read: shared with `kgraf_sandbox`'s inherited-environment scrub, so a name
/// is judged sensitive the same way in both places; each side of the sandbox
/// boundary should raise it here, not add a second copy.
///
/// Matched as a whole word, not a substring: `"pat"` (a personal access
/// token) must catch `GITLAB_PAT` without also catching `PATH` or `DB_PATH`,
/// and `"auth"` must catch `BASIC_AUTH` without also catching `AUTHOR`.
pub const SECRET_NAME_NEEDLES: &[&str] = &[
    "key",
    "token",
    "secret",
    "password",
    "passwd",
    "credential",
    "auth",
    "pat",
];

/// Whether `name` (an environment variable or a dotenv key) is judged to hold
/// a credential: one of its `_`/`-`/`.`/space-separated words matches a
/// [`SECRET_NAME_NEEDLES`] entry exactly, case-insensitive. Word-based, not a
/// raw substring test, so a needle chosen for a short, common abbreviation
/// (`"pat"`, `"auth"`) does not also fire on an unrelated word that merely
/// contains it (`PATH`, `AUTHOR`).
pub fn name_is_credential_shaped(name: &str) -> bool {
    name.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .any(|word| {
            SECRET_NAME_NEEDLES
                .iter()
                .any(|needle| word.eq_ignore_ascii_case(needle))
        })
}

/// The shortest exact secret value the registry acts on. Matches
/// `kgraf_mcp::http::MIN_SECRET_CHARS`'s reasoning exactly: a shorter string
/// (`"2.0"`, an empty API key placeholder) is ordinary text, and removing every
/// occurrence of it would corrupt unrelated output rather than protect anything.
pub const MIN_EXACT_SECRET_LEN: usize = 8;

/// `generic-api-key`'s own entropy floor (bits per byte of the captured
/// value), the only rule this crate enforces one for; see the module docs and
/// `third_party/gitleaks/UPSTREAM`.
const GENERIC_API_KEY_ENTROPY_FLOOR: f64 = 3.5;

/// The entropy floor a value must clear to join the exact-secret registry,
/// scaled to its length: `min(3.0, 0.5 * log2(len))`
/// bits per byte. A flat 3.5 (the first fix) refused most real short keys,
/// since a short string cannot reach high entropy: on 20,000 random keys per
/// length it refused 84% of 16-hex keys and 19% of 32-hex keys. This floor
/// refuses at most 0.04% of 8-to-40-character hex keys and 0.18% of
/// 8-character alphanumeric ones, and still refuses repeats such as
/// `aaaaaaaa` and `abababab`. Words and placeholders are refused by shape and
/// by name instead ([`is_placeholder`]).
pub fn min_exact_secret_entropy(len: usize) -> f64 {
    (0.5 * (len.max(1) as f64).log2()).min(3.0)
}

/// Values configured where a key is required but not checked: local servers
/// and examples. Compared case-insensitively. A word-shaped value is refused
/// too (see [`is_placeholder`]).
const PLACEHOLDERS: &[&str] = &[
    "lm-studio",
    "lmstudio",
    "ollama",
    "sk-no-key-required",
    "no-key-required",
    "not-needed",
    "changeme",
    "placeholder",
    "your-api-key",
    "your_api_key",
    "api-key-here",
    "dummy-key",
    "test-key",
];

/// Whether `value` is a known placeholder, or has the shape of a word rather
/// than a key: at most 20 characters of one-case letters and `._-`, no digit.
fn is_placeholder(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    if PLACEHOLDERS.iter().any(|p| *p == lower) {
        return true;
    }
    let sep = |c: char| c == '.' || c == '_' || c == '-';
    let lower_word = |w: &str| w.chars().all(|c| c.is_ascii_lowercase() || sep(c));
    // Title case: `Microsoft`, `Production`, `Kerberos`.
    let title_word = |w: &str| {
        let mut chars = w.chars();
        chars.next().is_some_and(|c| c.is_ascii_uppercase()) && lower_word(chars.as_str())
    };
    value.len() <= 20
        && (lower_word(value)
            || title_word(value)
            || value.chars().all(|c| c.is_ascii_uppercase() || sep(c)))
}

/// Why `value` is an ordinary configuration value rather than a key (final
/// review, A): a date, a short number, an absolute path, a URL with no
/// userinfo (a credential inside a URL is the URL rules' job), or a cloud
/// region. Registered, any of these would be redacted from model input too.
fn ordinary_value(value: &str) -> Option<&'static str> {
    let b = value.as_bytes();
    let digits =
        |r: std::ops::Range<usize>| b.get(r).is_some_and(|d| d.iter().all(u8::is_ascii_digit));
    let date_sep = |i: usize| matches!(b.get(i), Some(b'-' | b'/'));
    if digits(0..4) && date_sep(4) && digits(5..7) && date_sep(7) && digits(8..10) {
        return Some("it looks like a date");
    }
    if b.iter().all(u8::is_ascii_digit) && b.len() < 16 {
        return Some("it is a short number");
    }
    let windows_drive =
        b.len() > 2 && b[0].is_ascii_alphabetic() && b[1] == b':' && matches!(b[2], b'\\' | b'/');
    if value.starts_with('/')
        || value.starts_with("~/")
        || value.starts_with("\\\\")
        || windows_drive
    {
        return Some("it looks like a file path");
    }
    if let Some((_, rest)) = value.split_once("://") {
        let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
        if !authority.contains('@') {
            return Some("it looks like a URL");
        }
    }
    if is_region(value) {
        return Some("it looks like a cloud region");
    }
    None
}

/// `us-east-1a`, `eu-central-1`, `europe-west4`: lowercase words joined by
/// `-`, ending in one or two digits and an optional zone letter.
fn is_region(value: &str) -> bool {
    if value.len() > 24 || !value.contains('-') {
        return false;
    }
    let v = value
        .strip_suffix(|c: char| c.is_ascii_lowercase())
        .filter(|v| v.ends_with(|c: char| c.is_ascii_digit()))
        .unwrap_or(value);
    let head = v.trim_end_matches(|c: char| c.is_ascii_digit());
    let digits = v.len() - head.len();
    (1..=2).contains(&digits)
        && !head.is_empty()
        && head.chars().all(|c| c.is_ascii_lowercase() || c == '-')
        && head.split('-').filter(|w| !w.is_empty()).count() >= 2
}

/// What [`Scrubber::register_exact_secret`] did with a value. Never carries
/// the value itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Registration {
    /// Now scrubbed on both paths.
    Registered,
    /// Already registered; nothing changed.
    AlreadyKnown,
    /// Shorter than [`MIN_EXACT_SECRET_LEN`]: ordinary text (an empty key
    /// for a route that takes none), not scrubbed, not reported.
    TooShort,
    /// A placeholder or a word, not a key (see [`is_placeholder`]): not
    /// scrubbed, and reported (see [`Scrubber::take_refusal_notices`]).
    Placeholder,
    /// Below [`min_exact_secret_entropy`]: a repeat, not a key. Not scrubbed,
    /// and reported.
    LowEntropy,
    /// An ordinary configuration value (a date, a short number, a path, a
    /// URL, a region; see [`ordinary_value`]). Not scrubbed, and reported.
    Ordinary,
}

/// Per-kind redaction counts from one or more scrubs.
/// Holds kinds and counts only, never any of the redacted text, so it is safe
/// to write to a transcript or a trace.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Redactions(BTreeMap<String, u32>);

impl Redactions {
    /// No redaction was made.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Total redactions over every kind.
    pub fn total(&self) -> u64 {
        self.0.values().map(|&n| u64::from(n)).sum()
    }

    /// Kind to count, in kind order.
    pub fn counts(&self) -> &BTreeMap<String, u32> {
        &self.0
    }

    /// Add `other`'s counts into these.
    pub fn merge(&mut self, other: &Redactions) {
        for (kind, n) in &other.0 {
            *self.0.entry(kind.clone()).or_insert(0) += n;
        }
    }

    /// Consume into the kind-to-count map.
    pub fn into_counts(self) -> BTreeMap<String, u32> {
        self.0
    }

    fn add(&mut self, kind: &str) {
        *self.0.entry(kind.to_owned()).or_insert(0) += 1;
    }
}

/// One compiled pattern rule (see [`rules::PatternRule`] for the source data).
struct CompiledRule {
    kind: &'static str,
    regex: Regex,
    capture: Option<usize>,
}

/// A secret scrubber: gitleaks-derived pattern rules plus a process-local exact-
/// secret registry. Cheap to construct (once) and cheap to share (`&Scrubber` is
/// `Sync`); [`global`] holds kGraf's single production instance.
pub struct Scrubber {
    set: RegexSet,
    compiled: Vec<CompiledRule>,
    /// (kind, value), longest value first: a shorter registered secret that
    /// happens to be a substring of a longer one (unlikely, but not impossible
    /// for two related credentials) must not shadow the longer, more specific
    /// replacement.
    exact: RwLock<Vec<(String, String)>>,
    /// Values already refused, so each refusal is said once, not on every
    /// provider build. Placeholders by definition, not credentials.
    refused: Mutex<Vec<String>>,
    /// Where a refusal is said: the CLI prints it, the shell shows it.
    /// Until a sink is set, refusals wait in `pending_refusals`.
    refusal_sink: Mutex<Option<RefusalSink>>,
    pending_refusals: Mutex<Vec<String>>,
    /// This process's intake tag prefix, random per process; see
    /// [`Scrubber::intake`].
    nonce: u32,
    /// Intake tags minted so far; the next tag is this plus one.
    minted: AtomicU64,

    /// The model copies of texts stored persist-scrubbed (see [`intake`]).
    model_copies: Mutex<ModelCopies>,
    generic_api_key_match_allowlist: Vec<Regex>,
    generic_api_key_line_allowlist: Vec<Regex>,
}

impl Default for Scrubber {
    fn default() -> Self {
        Self::new()
    }
}

impl Scrubber {
    /// Compile every rule in [`rules::RULES`] once. Panics only on a rule whose
    /// pattern does not compile, which is a defect in `rules.rs` itself (every
    /// rule there is covered by a compiling, matching test in this module) --
    /// not a condition a caller can hit at runtime by any input it controls, so
    /// this is a construction-time invariant, not a place to thread a
    /// `Result` through every caller of [`global`].
    pub fn new() -> Self {
        let patterns: Vec<&str> = rules::RULES.iter().map(|r| r.pattern).collect();
        // Every secret shape here is ASCII (gitleaks itself only ever matches
        // ASCII credential alphabets), so Unicode-aware `\w`/case-folding
        // tables buy nothing and are what pushed the combined 44-pattern
        // automaton well past the default 10 MiB program-size limit: with
        // Unicode on, one `(?i)` rule alone folds over the whole Unicode
        // case-mapping table, and `RegexSet` has to keep that expansion
        // distinguishable per rule. `.unicode(false)` restricts `\w`, `\s`,
        // case-insensitivity and `.` to their ASCII meaning, which is exactly
        // gitleaks' own matching behavior (Go's `regexp` package has no
        // separate ASCII mode; its default *is* what this sets), and brings
        // the compiled program back under a modest, one-time limit paid once
        // at `global()`'s first call, never per scrub.
        let set = RegexSetBuilder::new(&patterns)
            .unicode(false)
            .size_limit(16 * 1024 * 1024)
            .build()
            .unwrap_or_else(|e| {
                panic!("kgraf-secrets::rules::RULES contains a pattern that does not compile: {e}")
            });
        let compiled = rules::RULES
            .iter()
            .map(|r| CompiledRule {
                kind: r.kind,
                regex: regex::RegexBuilder::new(r.pattern)
                    .unicode(false)
                    .build()
                    .unwrap_or_else(|e| {
                        panic!(
                            "rule `{}` compiled in the RegexSet but not alone: {e}",
                            r.kind
                        )
                    }),
                capture: r.capture,
            })
            .collect();
        let compile_all = |patterns: &[&str], label: &str| -> Vec<Regex> {
            patterns
                .iter()
                .map(|p| {
                    Regex::new(p)
                        .unwrap_or_else(|e| panic!("{label} pattern `{p}` does not compile: {e}"))
                })
                .collect()
        };
        Self {
            set,
            compiled,
            exact: RwLock::new(Vec::new()),
            refused: Mutex::new(Vec::new()),
            refusal_sink: Mutex::new(None),
            pending_refusals: Mutex::new(Vec::new()),
            nonce: process_nonce(),
            minted: AtomicU64::new(0),
            model_copies: Mutex::new(ModelCopies::default()),
            generic_api_key_match_allowlist: compile_all(
                generic_api_key_allowlist::MATCH_ALLOWLIST_REGEXES,
                "generic-api-key match-allowlist",
            ),
            generic_api_key_line_allowlist: compile_all(
                generic_api_key_allowlist::LINE_ALLOWLIST_REGEXES,
                "generic-api-key line-allowlist",
            ),
        }
    }

    /// Register a secret value kGraf itself holds (a provider API key, an MCP
    /// header's resolved environment value) so every future scrub call removes
    /// it, on both paths. Additive and process-lifetime, matching where
    /// credentials are read once at connect/build time (`kgraf-fleet`,
    /// `kgraf-mcp::http::resolve_headers`) and then used for the life of the
    /// process.
    ///
    /// Refused: a value under [`MIN_EXACT_SECRET_LEN`] (silently: an empty
    /// key is normal), a placeholder or word ([`is_placeholder`]), and a value
    /// under [`min_exact_secret_entropy`]. The last two are said (fail loud): once per value, naming the kind, never
    /// the value, through the refusal sink ([`set_refusal_sink`]) or, until one
    /// is set, [`Scrubber::take_refusal_notices`], and as a `tracing` warning.
    ///
    /// A poisoned lock (a panic elsewhere while holding it) is recovered rather
    /// than propagated: the registry's data is still valid, only the poison
    /// flag from an unrelated panic would otherwise stop every future
    /// registration and every future scrub, which is a worse failure than
    /// carrying on with a lock that is provably still consistent (`Vec::push`
    /// and the sort below cannot panic on this data).
    pub fn register_exact_secret(&self, kind: &str, value: &str) -> Registration {
        self.register(kind, value, true)
    }

    /// Register a dotenv file's text (`NAME=value` lines, `export ` prefixes,
    /// quotes and `#` comments understood) as exact secrets of kind `dotenv`,
    /// and return how many were newly registered. Only a pair worth treating
    /// as a credential is even offered to the registry: its name contains one
    /// of [`SECRET_NAME_NEEDLES`] (`API_KEY`, `GH_TOKEN`, `DB_PASSWORD`, ...),
    /// or its value matches one of this crate's own gitleaks-derived pattern
    /// rules regardless of its name. Everything else -- a target triple, a
    /// model name, an IP address, a log filter, a relative path, a toolchain
    /// date -- is ordinary configuration and is never registered, so it is
    /// never redacted from unrelated text that happens to contain the same
    /// bytes. The same refusals as [`Scrubber::register_exact_secret`] apply
    /// to what is offered, but are not said: a dotenv file's ordinary values
    /// are filtered out above, not reported as a credential kGraf will not
    /// scrub. No value is ever logged.
    ///
    /// Why (decision 0084): a checkout-root `.env` usually holds the user's
    /// vendor keys. A worker's build output reaches its model provider; the
    /// sandbox keeps the file unreadable, and this keeps the values out of
    /// every tool result, transcript and trace should one appear anyway.
    pub fn register_dotenv(&self, text: &str) -> usize {
        dotenv_pairs(text)
            .filter(|(name, value)| self.dotenv_worth_registering(name, value))
            .filter(|(_, value)| self.register("dotenv", value, false) == Registration::Registered)
            .count()
    }

    /// Whether a dotenv pair looks like a credential: its name is
    /// [`SECRET_NAME_NEEDLES`]-shaped, or its value matches a pattern rule on
    /// its own (an AWS key, a bare hex token, ... regardless of the name it
    /// was assigned to).
    fn dotenv_worth_registering(&self, name: &str, value: &str) -> bool {
        name_is_credential_shaped(name) || !self.pattern_spans(value).is_empty()
    }

    fn register(&self, kind: &str, value: &str, report: bool) -> Registration {
        let value = value.trim();
        let len = value.chars().count();
        if len < MIN_EXACT_SECRET_LEN {
            return Registration::TooShort;
        }
        let refusal = if is_placeholder(value) {
            Some((
                Registration::Placeholder,
                "it looks like a placeholder or a word",
            ))
        } else if let Some(why) = ordinary_value(value) {
            Some((Registration::Ordinary, why))
        } else if entropy::shannon_entropy(value) < min_exact_secret_entropy(len) {
            Some((Registration::LowEntropy, "it repeats too much to be a key"))
        } else {
            None
        };
        if let Some((outcome, why)) = refusal {
            if report {
                self.refuse(kind, value, why);
            }
            return outcome;
        }
        let mut secrets = self.exact.write().unwrap_or_else(|e| e.into_inner());
        if secrets.iter().any(|(_, v)| v == value) {
            return Registration::AlreadyKnown;
        }
        secrets.push((kind.to_owned(), value.to_owned()));
        secrets.sort_by_key(|(_, v)| std::cmp::Reverse(v.len()));
        Registration::Registered
    }

    /// Say a refusal once per value, without the value.
    fn refuse(&self, kind: &str, value: &str, why: &str) {
        {
            let mut refused = self.refused.lock().unwrap_or_else(|e| e.into_inner());
            if refused.iter().any(|v| v == value) {
                return;
            }
            refused.push(value.to_owned());
        }
        let notice = format!(
            "kGraf is not scrubbing a configured `{kind}` value from transcripts: {why}. \
             If it is a real credential, replace it with a random one."
        );
        tracing::warn!(kind, "{notice}");
        let sink = self.refusal_sink.lock().unwrap_or_else(|e| e.into_inner());
        match sink.as_ref() {
            Some(say) => say(&notice),
            None => self
                .pending_refusals
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(notice),
        }
    }

    /// Set where refusals are said, and say any that waited for it.
    pub fn set_refusal_sink(&self, sink: RefusalSink) {
        for notice in self.take_refusal_notices() {
            sink(&notice);
        }
        *self.refusal_sink.lock().unwrap_or_else(|e| e.into_inner()) = Some(sink);
    }

    /// Refusals not yet said through a sink, oldest first. Each names the kind
    /// and why, never the value.
    pub fn take_refusal_notices(&self) -> Vec<String> {
        std::mem::take(
            &mut *self
                .pending_refusals
                .lock()
                .unwrap_or_else(|e| e.into_inner()),
        )
    }

    /// The model-facing path: only the exact-secret registry. See the module
    /// docs for why pattern rules never run here.
    pub fn scrub_model_facing<'a>(&self, text: &'a str) -> Cow<'a, str> {
        self.scrub_model_facing_counted(text).0
    }

    /// [`Scrubber::scrub_model_facing`], with what it redacted, by kind.
    pub fn scrub_model_facing_counted<'a>(&self, text: &'a str) -> (Cow<'a, str>, Redactions) {
        let spans = self.exact_spans(text);
        match self.apply_spans(text, spans, None) {
            Some((s, r)) => (Cow::Owned(s), r),
            None => (Cow::Borrowed(text), Redactions::default()),
        }
    }

    /// The persist/display path: the exact-secret registry, plus pattern
    /// rules (with `generic-api-key` additionally gated by entropy and
    /// gitleaks' own allowlist; see the module docs).
    pub fn scrub_persist<'a>(&self, text: &'a str) -> Cow<'a, str> {
        self.scrub_persist_counted(text).0
    }

    /// [`Scrubber::scrub_persist`], with what it redacted, by kind.
    /// The counts carry no secret text.
    pub fn scrub_persist_counted<'a>(&self, text: &'a str) -> (Cow<'a, str>, Redactions) {
        let spans = self.persist_spans(text);
        match self.apply_spans(text, spans, None) {
            Some((s, r)) => (Cow::Owned(s), r),
            None => (Cow::Borrowed(text), Redactions::default()),
        }
    }

    fn persist_spans(&self, text: &str) -> Vec<(usize, usize, String)> {
        let mut spans = self.exact_spans(text);
        spans.extend(self.pattern_spans(text));
        spans
    }

    /// Scrub `raw` for storage and display, and keep its model copy in memory.
    /// The returned string is the
    /// persist-scrubbed copy, which is what a caller stores, shows and
    /// indexes; [`Scrubber::model_copy`] later maps that stored string back to
    /// the registry-only scrub of `raw`, which is what a model is shown. Call
    /// it once, where text enters kGraf, never on text it already returned.
    ///
    /// **The stored copy carries its own id**. When
    /// a pattern rule redacts something the registry does not (the only case
    /// with a model copy to keep), every marker in the stored copy is tagged
    /// `[redacted:<kind>@<nonce>.<n>; ...]`: this process's random nonce and a
    /// counter never reused. The table is keyed by that stored text, so no two
    /// intakes share a key: a second text with the same shape gets its own
    /// tag, and an evicted key never comes back with other text. A tag in
    /// `raw` itself is stripped first, so text copied from a stored form (a
    /// committed conversation file, a shown record) cannot take its key.
    ///
    /// The model copy is held only in this process's memory, never written.
    /// After a restart, [`Scrubber::model_copy`] finds nothing and returns the
    /// stored copy, so a resumed run's model sees the markers.
    pub fn intake(&self, raw: &str) -> String {
        let raw = self.strip_own_tags(raw);
        let raw = raw.as_ref();
        let model = self.scrub_model_facing(raw);
        let (stored, _) = self.scrub_persist_counted(raw);
        if stored == model {
            return stored.into_owned();
        }
        let n = self.minted.fetch_add(1, Ordering::Relaxed) + 1;
        let tag = format!("{:08x}.{n}", self.nonce);
        let spans = self.persist_spans(raw);
        let Some((tagged, _)) = self.apply_spans(raw, spans, Some(&tag)) else {
            return stored.into_owned();
        };
        self.model_copies
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remember(&tagged, model.as_ref());
        tagged
    }

    /// The text a model is shown for `stored`, a string [`Scrubber::intake`]
    /// returned: its registry-only copy when this process still holds one,
    /// else `stored` itself. Never returns text the registry would redact.
    pub fn model_copy<'a>(&self, stored: &'a str) -> Cow<'a, str> {
        let copies = self.model_copies.lock().unwrap_or_else(|e| e.into_inner());
        match copies.get(stored) {
            Some(model) => Cow::Owned(model.to_owned()),
            None => Cow::Borrowed(stored),
        }
    }

    /// Drop the model copy of `stored`, when a caller no longer needs it (a
    /// run that ended). A model then reads the stored copy.
    pub fn forget_model_copy(&self, stored: &str) {
        self.model_copies
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .forget(stored);
    }

    /// `text` with every intake tag removed from its markers, so two stored
    /// copies of the same text compare equal whatever their tags. For
    /// comparisons only (a cycle guard); never a key.
    pub fn without_tags<'a>(&self, text: &'a str) -> Cow<'a, str> {
        let found = marker_shapes(text);
        if found.iter().all(|m| m.tag.is_none()) {
            return Cow::Borrowed(text);
        }
        let mut out = String::with_capacity(text.len());
        let mut cursor = 0;
        for m in found {
            if let Some((tag_start, tag_end)) = m.tag {
                // `tag_start - 1` is the `@`.
                out.push_str(&text[cursor..tag_start - 1]);
                cursor = tag_end;
            }
        }
        out.push_str(&text[cursor..]);
        Cow::Owned(out)
    }

    /// `raw` with any tag carrying this process's nonce removed (see
    /// [`Scrubber::intake`]).
    fn strip_own_tags<'a>(&self, raw: &'a str) -> Cow<'a, str> {
        if raw.contains(&format!("@{:08x}.", self.nonce)) {
            self.without_tags(raw)
        } else {
            Cow::Borrowed(raw)
        }
    }

    /// Whether `kind` is one this crate writes: a rule kind, or a kind a
    /// secret was registered under.
    fn is_known_kind(&self, kind: &str) -> bool {
        rules::RULES.iter().any(|r| r.kind == kind)
            || self
                .exact
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .any(|(k, _)| k == kind)
    }

    /// Whether `tag` is one this process minted: its nonce, and a counter it
    /// has reached, written without leading zeros.
    fn is_own_tag(&self, tag: &str) -> bool {
        let Some((nonce, n)) = tag.split_once('.') else {
            return false;
        };
        nonce == format!("{:08x}", self.nonce)
            && !n.starts_with('0')
            && n.parse::<u64>()
                .is_ok_and(|n| n >= 1 && n <= self.minted.load(Ordering::Relaxed))
    }

    /// Byte ranges of `text` that are markers this scrubber writes, which a
    /// later scrub must not land inside: a known kind, with no tag or
    /// one of this process's own. Any other lookalike, with a secret in the
    /// kind or the tag position, is scanned like other text.
    fn marker_ranges(&self, text: &str) -> Vec<(usize, usize)> {
        marker_shapes(text)
            .into_iter()
            .filter(|m| {
                self.is_known_kind(&text[m.kind.0..m.kind.1])
                    && m.tag.is_none_or(|(a, b)| self.is_own_tag(&text[a..b]))
            })
            .map(|m| (m.start, m.end))
            .collect()
    }

    /// Resolve every `(start, end, kind)` span against `text` in one pass:
    /// earliest start first, ties broken by the longer span; a span nested in
    /// the one before is skipped, and one that overlaps it past its end
    /// extends it to their union; any span that overlaps a marker this
    /// scrubber wrote is dropped. Each marker carries `tag` when one
    /// is given. `None` when nothing survives, so the caller can keep its
    /// input borrowed.
    fn apply_spans(
        &self,
        text: &str,
        mut spans: Vec<(usize, usize, String)>,
        tag: Option<&str>,
    ) -> Option<(String, Redactions)> {
        if spans.is_empty() {
            return None;
        }
        spans.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
        let existing_markers = self.marker_ranges(text);
        let mut out = String::with_capacity(text.len());
        let mut cursor = 0usize;
        let mut counts = Redactions::default();
        for (start, end, kind) in spans {
            if inside_any(&existing_markers, start)
                || end
                    .checked_sub(1)
                    .is_some_and(|last| inside_any(&existing_markers, last))
            {
                continue;
            }
            // A span that overlaps the one just written extends it to their
            // union: dropping it whole leaked the part past
            // the first span's end. One redaction, counted once.
            if start < cursor {
                cursor = cursor.max(end);
                continue;
            }
            out.push_str(&text[cursor..start]);
            match tag {
                Some(tag) => out.push_str(&marker(&format!("{kind}@{tag}"))),
                None => out.push_str(&marker(&kind)),
            }
            cursor = end;
            counts.add(&kind);
        }
        if counts.is_empty() {
            return None;
        }
        out.push_str(&text[cursor..]);
        Some((out, counts))
    }

    /// Every exact-registry match in `text`, as `(start, end, kind)` spans
    /// against the original text (never chained onto another pass's output,
    /// so this and [`Scrubber::pattern_spans`] can be merged and resolved by
    /// one call to [`apply_spans`]).
    fn exact_spans(&self, text: &str) -> Vec<(usize, usize, String)> {
        let secrets = self.exact.read().unwrap_or_else(|e| e.into_inner());
        if secrets.is_empty() {
            return Vec::new();
        }
        let mut spans = Vec::new();
        for (kind, value) in secrets.iter() {
            let mut cursor = 0usize;
            while let Some(rel) = text[cursor..].find(value.as_str()) {
                let start = cursor + rel;
                spans.push((start, start + value.len(), kind.clone()));
                cursor = start + value.len();
            }
        }
        spans
    }

    /// Every pattern-rule match in `text` that survives `generic-api-key`'s
    /// extra gate (every other rule's own structural regex is the only gate).
    fn pattern_spans(&self, text: &str) -> Vec<(usize, usize, String)> {
        if !self.set.is_match(text) {
            return Vec::new();
        }
        let matched = self.set.matches(text);
        let mut spans = Vec::new();
        for idx in matched.iter() {
            let rule = &self.compiled[idx];
            for caps in rule.regex.captures_iter(text) {
                // Group 0 always matches a successful capture; the `else`
                // only keeps this free of a panic path.
                let Some(m) = rule
                    .capture
                    .and_then(|g| caps.get(g))
                    .or_else(|| caps.get(0))
                else {
                    continue;
                };
                if rule.kind == "generic-api-key"
                    && !self.generic_api_key_allows(text, m.as_str(), m.start())
                {
                    continue;
                }
                spans.push((m.start(), m.end(), rule.kind.to_owned()));
            }
        }
        spans
    }

    /// `false` if `value` (found at byte offset `start` in `text`) is
    /// allowlisted, entropy-gated out, or a stopword hit for `generic-api-key`
    /// (ported from gitleaks, see `generic_api_key_allowlist`).
    fn generic_api_key_allows(&self, text: &str, value: &str, start: usize) -> bool {
        if entropy::shannon_entropy(value) < GENERIC_API_KEY_ENTROPY_FLOOR {
            return false;
        }
        if self
            .generic_api_key_match_allowlist
            .iter()
            .any(|r| r.is_match(value))
        {
            return false;
        }
        let lower = value.to_ascii_lowercase();
        if generic_api_key_allowlist::STOPWORDS
            .iter()
            .any(|w| lower.contains(w))
        {
            return false;
        }
        let line = line_containing(text, start);
        if self
            .generic_api_key_line_allowlist
            .iter()
            .any(|r| r.is_match(line))
        {
            return false;
        }
        true
    }
}

/// The self-describing redaction marker (see the module docs): keeps the
/// kind, never the value, and says a resume should re-read the source rather
/// than trust the marker as the original content.
fn marker(kind: &str) -> String {
    format!("[redacted:{kind}; re-read the source for exact text]")
}

/// The line of `text` containing byte offset `pos` (gitleaks' `regexTarget =
/// "line"` allowlists apply to the whole line, not just the match).
fn line_containing(text: &str, pos: usize) -> &str {
    let start = text[..pos].rfind('\n').map_or(0, |i| i + 1);
    let end = text[pos..].find('\n').map_or(text.len(), |i| pos + i);
    &text[start..end]
}

/// The fixed tail of every marker this crate writes, after the kind.
const MARKER_TAIL: &str = "; re-read the source for exact text]";

/// A complete marker-shaped span, `[redacted:<kind>[@<tag>]; re-read the
/// source for exact text]`, with the byte ranges of its kind and tag. Shape
/// only; [`Scrubber::marker_ranges`] decides which are this scrubber's own.
struct MarkerShape {
    start: usize,
    end: usize,
    kind: (usize, usize),
    tag: Option<(usize, usize)>,
}

fn marker_shapes(text: &str) -> Vec<MarkerShape> {
    const OPEN: &str = "[redacted:";
    let ident = |s: &str| {
        !s.is_empty()
            && s.len() <= 64
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
    };
    let mut found = Vec::new();
    let mut cursor = 0usize;
    while let Some(rel) = text[cursor..].find(OPEN) {
        let open = cursor + rel;
        let body = open + OPEN.len();
        let rest = &text[body..];
        // The `;` can only be within a kind, an `@` and a tag of at most 64
        // bytes each: searching further made the scan
        // quadratic in the number of openers.
        let window = &rest.as_bytes()[..rest.len().min(64 + 1 + 64 + 1)];
        let semi = window.iter().position(|&b| b == b';');
        let shape = semi.and_then(|semi| {
            if !rest[semi..].starts_with(MARKER_TAIL) {
                return None;
            }
            let head = &rest[..semi];
            let (kind, tag) = match head.split_once('@') {
                Some((k, t)) => (k, Some(t)),
                None => (head, None),
            };
            if !ident(kind) || tag.is_some_and(|t| !ident(t)) {
                return None;
            }
            let tag_start = body + kind.len() + 1;
            Some(MarkerShape {
                start: open,
                end: body + semi + MARKER_TAIL.len(),
                kind: (body, body + kind.len()),
                tag: tag.map(|t| (tag_start, tag_start + t.len())),
            })
        });
        match shape {
            Some(m) => {
                cursor = m.end;
                found.push(m);
            }
            None => cursor = body,
        }
    }
    found
}

fn inside_any(ranges: &[(usize, usize)], pos: usize) -> bool {
    ranges.iter().any(|&(s, e)| pos >= s && pos < e)
}

/// A random nonce per process, from the standard library's randomly keyed
/// hasher (no extra dependency), so intake tags from two processes differ.
fn process_nonce() -> u32 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u32(std::process::id());
    h.finish() as u32
}

/// Where refusals are said (see [`Scrubber::set_refusal_sink`]).
pub type RefusalSink = Box<dyn Fn(&str) + Send + Sync>;

/// The in-memory model copies behind [`Scrubber::intake`]: stored
/// (persist-scrubbed, tagged) text to its registry-only copy.
///
/// Why process-wide, not a context object: a stored text reaches a model
/// through several owners (the orchestrator's brief, a peer worker's
/// mailbox, a session inbox), and each would otherwise need the table
/// threaded to it. The table holds only what a model is shown anyway, in
/// memory, beside the registry that is already process-wide. Keys are unique
/// per intake (see [`Scrubber::intake`]), so sharing it across runs cannot
/// hand one text's copy to another.
///
/// Bounded at [`MODEL_COPY_CAP`] entries and [`MODEL_COPY_MAX_BYTES`] bytes,
/// oldest dropped first, and a run forgets its own copies when it ends
/// ([`forget_model_copy`]). An evicted, forgotten or never-held entry falls
/// back to the stored copy: the failure mode is a marker in model input,
/// never a secret on disk and never another text's copy.
#[derive(Default)]
struct ModelCopies {
    /// `None` when one key was remembered with two different copies. Keys are
    /// unique per intake, so this is a defense, not an expected state; such a
    /// key is dead and is never reopened while it is held.
    map: HashMap<String, Option<String>>,
    order: VecDeque<String>,
    bytes: usize,
}

/// Most model copies held at once. A few thousand user notes, mailbox bodies
/// and contract briefs with a pattern redaction in them is far past one run.
const MODEL_COPY_CAP: usize = 4096;

/// Most bytes of keys and copies held at once.
const MODEL_COPY_MAX_BYTES: usize = 16 * 1024 * 1024;

impl ModelCopies {
    fn remember(&mut self, stored: &str, model: &str) {
        if let Some(slot) = self.map.get_mut(stored) {
            if slot.as_deref() != Some(model) {
                *slot = None;
            }
            return;
        }
        let size = stored.len() + model.len();
        if size > MODEL_COPY_MAX_BYTES {
            return;
        }
        while self.map.len() >= MODEL_COPY_CAP || self.bytes + size > MODEL_COPY_MAX_BYTES {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.drop_entry(&oldest);
        }
        self.map.insert(stored.to_owned(), Some(model.to_owned()));
        self.order.push_back(stored.to_owned());
        self.bytes += size;
    }

    fn drop_entry(&mut self, stored: &str) {
        if let Some(copy) = self.map.remove(stored) {
            self.bytes -= stored.len() + copy.map_or(0, |c| c.len());
        }
    }

    fn forget(&mut self, stored: &str) {
        self.drop_entry(stored);
        self.order.retain(|k| k != stored);
    }

    fn get(&self, stored: &str) -> Option<&str> {
        self.map.get(stored).and_then(|m| m.as_deref())
    }
}

/// kGraf's one process-wide scrubber. Every worker's transcript, every MCP
/// header's resolved credential, and every provider API key funnel through
/// this instance, so a secret registered while assembling one worker's
/// environment is scrubbed from every other worker's transcript too -- which is
/// correct: the same credential in two workers' tool output is the same secret.
pub fn global() -> &'static Scrubber {
    static GLOBAL: OnceLock<Scrubber> = OnceLock::new();
    GLOBAL.get_or_init(Scrubber::new)
}

/// Register a secret kGraf itself holds with the process-wide scrubber (see
/// [`Scrubber::register_exact_secret`]). Call this at the point the secret is
/// read from configuration or the environment: `kgraf-fleet`'s provider
/// credential, `kgraf-mcp::http::resolve_headers`'s resolved header value.
pub fn register_exact_secret(kind: &str, value: &str) -> Registration {
    global().register_exact_secret(kind, value)
}

/// Register a dotenv file's values with the process-wide scrubber (see
/// [`Scrubber::register_dotenv`]). A missing file registers nothing; any
/// other read error is returned. The values are never logged.
pub fn register_dotenv_file(path: &std::path::Path) -> std::io::Result<usize> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(global().register_dotenv(&text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(e) => Err(e),
    }
}

/// The `(name, value)` pairs of a dotenv file's `NAME=value` lines: `export `
/// prefixes dropped, lines without `=` and comment lines skipped. A value
/// that opens with a quote ends at its *matching* closing quote -- whatever
/// follows, quoted or not, is discarded, since a quoted value cannot contain
/// an unescaped quote of its own kind and gitleaks-shaped dotenv writers
/// never escape one. An unquoted value keeps everything up to a literal
/// ` #` (a `#` with no leading space is common inside a URL fragment or a
/// password, so it is not treated as a comment marker there).
fn dotenv_pairs(text: &str) -> impl Iterator<Item = (&str, &str)> {
    text.lines().filter_map(|line| {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return None;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let (name, raw_value) = line.split_once('=')?;
        let name = name.trim();
        let raw_value = raw_value.trim();
        let value = match raw_value.chars().next() {
            Some(quote @ ('"' | '\'')) => {
                let rest = &raw_value[quote.len_utf8()..];
                match rest.find(quote) {
                    Some(end) => &rest[..end],
                    None => rest,
                }
            }
            _ => raw_value.split(" #").next().unwrap_or(raw_value).trim(),
        };
        Some((name, value))
    })
}

/// The model-facing path with the process-wide scrubber (see
/// [`Scrubber::scrub_model_facing`]).
pub fn scrub_model_facing(text: &str) -> Cow<'_, str> {
    global().scrub_model_facing(text)
}

/// The persist/display path with the process-wide scrubber (see
/// [`Scrubber::scrub_persist`]).
pub fn scrub_persist(text: &str) -> Cow<'_, str> {
    global().scrub_persist(text)
}

/// The persist/display path with its counts (see
/// [`Scrubber::scrub_persist_counted`]).
pub fn scrub_persist_counted(text: &str) -> (Cow<'_, str>, Redactions) {
    global().scrub_persist_counted(text)
}

/// [`scrub_persist`] into an owned `String`, reusing `text` when nothing was
/// redacted. For callers that hold the text by value.
pub fn scrub_persist_owned(text: String) -> String {
    match global().scrub_persist(&text) {
        Cow::Borrowed(_) => text,
        Cow::Owned(scrubbed) => scrubbed,
    }
}

/// Scrub for storage and keep the model copy in memory (see
/// [`Scrubber::intake`]).
pub fn intake(raw: &str) -> String {
    global().intake(raw)
}

/// The model's copy of a stored text (see [`Scrubber::model_copy`]).
pub fn model_copy(stored: &str) -> Cow<'_, str> {
    global().model_copy(stored)
}

/// Drop a stored text's model copy (see [`Scrubber::forget_model_copy`]).
pub fn forget_model_copy(stored: &str) {
    global().forget_model_copy(stored)
}

/// A stored text with its intake tags removed, for comparison (see
/// [`Scrubber::without_tags`]).
pub fn without_tags(text: &str) -> Cow<'_, str> {
    global().without_tags(text)
}

/// Set where the process-wide scrubber's refusals are said (see
/// [`Scrubber::set_refusal_sink`]). The CLI prints them to stderr.
pub fn set_refusal_sink(sink: RefusalSink) {
    global().set_refusal_sink(sink)
}

/// Refusals the process-wide scrubber has not said yet (see
/// [`Scrubber::take_refusal_notices`]). The shell shows them as notices.
pub fn take_refusal_notices() -> Vec<String> {
    global().take_refusal_notices()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// A fresh scrubber per test: no test shares a `Scrubber` (unlike
    /// [`global`]) so a test that registers an exact secret cannot leak state
    /// into another test via a shared process-wide singleton.
    fn fresh() -> Scrubber {
        Scrubber::new()
    }

    /// A high-entropy filler of exactly `n` ASCII letters and digits, cycling
    /// through a 32-symbol alphabet: a real secret's value is close to random
    /// over its alphabet, and `"a".repeat(n)` (zero entropy) is exactly the
    /// kind of value `generic-api-key`'s entropy gate exists to reject, so a
    /// test secret needs to look like one to exercise the persist path
    /// honestly. Every pattern-rule test in this module that redacts a
    /// `generic-api-key` match uses this; rules with their own fixed prefix
    /// and length are not entropy-gated (see `rules::PatternRule::entropy`'s
    /// doc) and keep using a repeated character, which is easier to read and
    /// exercises the same code path just as well.
    fn filler(n: usize) -> String {
        const ALPHABET: &[u8] = b"aB3xZ9qM7wK1pL5vN2tR8yH4jC6dF0sG";
        (0..n)
            .map(|i| ALPHABET[i % ALPHABET.len()] as char)
            .collect()
    }

    fn assert_redacted_persist(scrubber: &Scrubber, text: &str, kind: &str, secret_fragment: &str) {
        let out = scrubber.scrub_persist(text);
        assert!(
            out.contains(&format!("[redacted:{kind};")),
            "expected `{kind}` to redact {text:?} on the persist path, got {out:?}"
        );
        assert!(
            !out.contains(secret_fragment),
            "the secret itself must not survive redaction: {out:?}"
        );
    }

    fn assert_not_redacted_persist(scrubber: &Scrubber, text: &str) {
        let out = scrubber.scrub_persist(text);
        assert_eq!(
            &*out, text,
            "a benign string must pass through the persist path unchanged"
        );
    }

    // -- Pattern rules: one true and one false case each, on the persist path
    //    (model-facing tests are further down) --------

    #[test]
    fn anthropic_admin_api_key() {
        let s = fresh();
        let secret = format!("sk-ant-admin01-{}AA", filler(93));
        assert_redacted_persist(
            &s,
            &format!("see {secret} end"),
            "anthropic-admin-api-key",
            &secret,
        );
        let too_short = format!("sk-ant-admin01-{}AA", "a".repeat(10));
        assert_not_redacted_persist(&s, &format!("see {too_short} end"));
    }

    #[test]
    fn anthropic_api_key() {
        let s = fresh();
        let secret = format!("sk-ant-api03-{}AA", filler(93));
        assert_redacted_persist(
            &s,
            &format!("see {secret} end"),
            "anthropic-api-key",
            &secret,
        );
        let too_short = format!("sk-ant-api03-{}AA", "a".repeat(10));
        assert_not_redacted_persist(&s, &format!("see {too_short} end"));
    }

    #[test]
    fn openai_api_key() {
        let s = fresh();
        let secret = format!("sk-{}T3BlbkFJ{}", filler(20), filler(20));
        assert_redacted_persist(
            &s,
            &format!("export OPENAI_API_KEY={secret}\n"),
            "openai-api-key",
            &secret,
        );
        assert_not_redacted_persist(&s, "the marker T3BlbkFJ appears mid-sentence, alone");
    }

    #[test]
    fn cohere_api_token() {
        let s = fresh();
        let value = filler(40);
        assert_redacted_persist(
            &s,
            &format!("cohere_api_key=\"{value}\""),
            "cohere-api-token",
            &value,
        );
        assert_not_redacted_persist(&s, "cohere is a company that makes language models");
    }

    #[test]
    fn huggingface_access_token() {
        let s = fresh();
        let secret = format!("hf_{}", "a".repeat(34));
        assert_redacted_persist(
            &s,
            &format!("see {secret} "),
            "huggingface-access-token",
            &secret,
        );
        assert_not_redacted_persist(&s, "see hf_short ");
    }

    #[test]
    fn huggingface_organization_api_token() {
        let s = fresh();
        let secret = format!("api_org_{}", "a".repeat(34));
        assert_redacted_persist(
            &s,
            &format!("see {secret} "),
            "huggingface-organization-api-token",
            &secret,
        );
        assert_not_redacted_persist(&s, "see api_org_short ");
    }

    #[test]
    fn perplexity_api_key() {
        let s = fresh();
        let secret = format!("pplx-{}", "a".repeat(48));
        assert_redacted_persist(&s, &format!("see {secret} "), "perplexity-api-key", &secret);
        assert_not_redacted_persist(&s, "see pplx-short ");
    }

    #[test]
    fn aws_access_token() {
        let s = fresh();
        let secret = format!("AKIA{}", "A".repeat(16));
        assert_redacted_persist(
            &s,
            &format!("aws key {secret} in file"),
            "aws-access-token",
            &secret,
        );
        assert_not_redacted_persist(
            &s,
            &format!("aws key {} in file", secret.to_ascii_lowercase()),
        );
    }

    #[test]
    fn gcp_api_key() {
        let s = fresh();
        let secret = format!("AIza{}", "a".repeat(35));
        assert_redacted_persist(&s, &format!("key: {secret} "), "gcp-api-key", &secret);
        assert_not_redacted_persist(&s, "key: AIzashort ");
    }

    #[test]
    fn azure_ad_client_secret() {
        let s = fresh();
        let secret = format!("abc5Q~{}", "x".repeat(32));
        assert_redacted_persist(
            &s,
            &format!(" {secret} "),
            "azure-ad-client-secret",
            &secret,
        );
        let too_short = format!("abc5Q~{}", "x".repeat(10));
        assert_not_redacted_persist(&s, &format!(" {too_short} "));
    }

    #[test]
    fn digitalocean_access_token() {
        let s = fresh();
        let secret = format!("doo_v1_{}", "a".repeat(64));
        assert_redacted_persist(
            &s,
            &format!("see {secret} "),
            "digitalocean-access-token",
            &secret,
        );
        assert_not_redacted_persist(&s, "see doo_v1_short ");
    }

    #[test]
    fn digitalocean_pat() {
        let s = fresh();
        let secret = format!("dop_v1_{}", "a".repeat(64));
        assert_redacted_persist(&s, &format!("see {secret} "), "digitalocean-pat", &secret);
        assert_not_redacted_persist(&s, "see dop_v1_short ");
    }

    #[test]
    fn github_pat() {
        let s = fresh();
        let secret = format!("ghp_{}", "a".repeat(36));
        assert_redacted_persist(&s, &format!("see {secret} "), "github-pat", &secret);
        assert_not_redacted_persist(&s, "see ghp_short ");
    }

    #[test]
    fn github_fine_grained_pat() {
        let s = fresh();
        let secret = format!("github_pat_{}", "a".repeat(82));
        assert_redacted_persist(
            &s,
            &format!("see {secret} "),
            "github-fine-grained-pat",
            &secret,
        );
        assert_not_redacted_persist(&s, "see github_pat_short ");
    }

    #[test]
    fn github_oauth() {
        let s = fresh();
        let secret = format!("gho_{}", "a".repeat(36));
        assert_redacted_persist(&s, &format!("see {secret} "), "github-oauth", &secret);
        assert_not_redacted_persist(&s, "see gho_short ");
    }

    #[test]
    fn github_app_token() {
        let s = fresh();
        let secret = format!("ghu_{}", "a".repeat(36));
        assert_redacted_persist(&s, &format!("see {secret} "), "github-app-token", &secret);
        assert_not_redacted_persist(&s, "see ghu_short ");
    }

    #[test]
    fn github_refresh_token() {
        let s = fresh();
        let secret = format!("ghr_{}", "a".repeat(36));
        assert_redacted_persist(
            &s,
            &format!("see {secret} "),
            "github-refresh-token",
            &secret,
        );
        assert_not_redacted_persist(&s, "see ghr_short ");
    }

    #[test]
    fn gitlab_pat() {
        let s = fresh();
        let secret = format!("glpat-{}", "a".repeat(20));
        assert_redacted_persist(&s, &format!("see {secret} "), "gitlab-pat", &secret);
        assert_not_redacted_persist(&s, "see glpat-short ");
    }

    #[test]
    fn gitlab_pat_routable() {
        let s = fresh();
        let secret = format!("glpat-{}.{}", "a".repeat(27), "a".repeat(9));
        assert_redacted_persist(
            &s,
            &format!("see {secret} "),
            "gitlab-pat-routable",
            &secret,
        );
        let too_short = format!("glpat-{}.{}", "a".repeat(5), "a".repeat(9));
        assert_not_redacted_persist(&s, &format!("see {too_short} "));
    }

    #[test]
    fn generic_api_key() {
        let s = fresh();
        let value = filler(20);
        assert_redacted_persist(
            &s,
            &format!("api_key = \"{value}\""),
            "generic-api-key",
            &value,
        );
        // A hash in a git log is not a key.
        assert_not_redacted_persist(&s, "commit a1b2c3d4e5f6 by Jane <jane@example.com>");
        assert_not_redacted_persist(&s, "the cache-key for this workflow is main-branch");
        // The exact false positive the security review found: a low-entropy
        // assignment whose value is an ordinary identifier.
        assert_not_redacted_persist(&s, "stats.code_tokens = code_spent;");
    }

    #[test]
    fn jwt() {
        let s = fresh();
        let secret = format!(
            "eyJ{}.eyJ{}.{}",
            "a".repeat(20),
            "a".repeat(20),
            "a".repeat(15)
        );
        assert_redacted_persist(&s, &format!("bearer {secret} "), "jwt", &secret);
        assert_not_redacted_persist(&s, "eyeglasses make good eyewear");
    }

    #[test]
    fn jwt_base64() {
        let s = fresh();
        let secret = format!("ZXlKaGJHY2lPaU{}", "a".repeat(45));
        assert_redacted_persist(&s, &format!("see {secret} "), "jwt-base64", &secret);
        assert_not_redacted_persist(&s, "see ZXlK short, not a real jwt-base64 body");
    }

    #[test]
    fn private_key() {
        let s = fresh();
        let secret = format!(
            "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----",
            "a".repeat(70)
        );
        assert_redacted_persist(&s, &secret, "private-key", &secret);
        assert_not_redacted_persist(
            &s,
            "we discussed the private key rotation policy in private-key.md",
        );
    }

    #[test]
    fn slack_webhook_url() {
        let s = fresh();
        let secret = format!("https://hooks.slack.com/services/{}", "a".repeat(45));
        assert_redacted_persist(&s, &secret, "slack-webhook-url", &secret);
        assert_not_redacted_persist(&s, "we post build notices to hooks.slack.com");
    }

    #[test]
    fn slack_bot_token() {
        let s = fresh();
        let secret = "xoxb-1234567890-1234567890-abcdefghijklmnopqrstuvwxyz".to_owned();
        assert_redacted_persist(&s, &secret, "slack-bot-token", &secret);
        assert_not_redacted_persist(&s, "xoxb-123-456 is too short to be a real token");
    }

    #[test]
    fn slack_legacy_token() {
        let s = fresh();
        let secret = "xoxo-1-2-3-abcdef1234".to_owned();
        assert_redacted_persist(&s, &secret, "slack-legacy-token", &secret);
        assert_not_redacted_persist(&s, "xoxa-1-2-3-not-a-legacy-scheme");
    }

    #[test]
    fn discord_api_token() {
        let s = fresh();
        let value = "a".repeat(64);
        assert_redacted_persist(
            &s,
            &format!("discord_token=\"{value}\""),
            "discord-api-token",
            &value,
        );
        assert_not_redacted_persist(&s, "discord is a chat app popular with gamers");
    }

    #[test]
    fn stripe_access_token() {
        let s = fresh();
        let secret = format!("sk_live_{}", "a".repeat(20));
        assert_redacted_persist(
            &s,
            &format!("see {secret} "),
            "stripe-access-token",
            &secret,
        );
        assert_not_redacted_persist(&s, "see sk_live_a ");
    }

    #[test]
    fn sidekiq_sensitive_url() {
        let s = fresh();
        let secret = format!(
            "https://{}:{}@gems.contribsys.com/",
            "a".repeat(8),
            "b".repeat(8)
        );
        assert_redacted_persist(&s, &secret, "sidekiq-sensitive-url", &secret);
        assert_not_redacted_persist(&s, "https://example.com/status");
    }

    #[test]
    fn npm_access_token() {
        let s = fresh();
        let secret = format!("npm_{}", "a".repeat(36));
        assert_redacted_persist(&s, &format!("see {secret} "), "npm-access-token", &secret);
        assert_not_redacted_persist(&s, "see npm_short ");
    }

    #[test]
    fn pypi_upload_token() {
        let s = fresh();
        let secret = format!("pypi-AgEIcHlwaS5vcmc{}", "a".repeat(60));
        assert_redacted_persist(&s, &secret, "pypi-upload-token", &secret);
        assert_not_redacted_persist(&s, "pypi-AgEIcHlwaS5vcmc-too-short");
    }

    #[test]
    fn rubygems_api_token() {
        let s = fresh();
        let secret = format!("rubygems_{}", "a".repeat(48));
        assert_redacted_persist(&s, &format!("see {secret} "), "rubygems-api-token", &secret);
        assert_not_redacted_persist(&s, "see rubygems_short ");
    }

    #[test]
    fn hashicorp_tf_api_token() {
        let s = fresh();
        let secret = format!("{}.atlasv1.{}", "a".repeat(14), "a".repeat(65));
        assert_redacted_persist(&s, &secret, "hashicorp-tf-api-token", &secret);
        let too_short = format!("{}.atlasv1.{}", "a".repeat(5), "a".repeat(65));
        assert_not_redacted_persist(&s, &too_short);
    }

    #[test]
    fn vault_service_token() {
        let s = fresh();
        let secret = format!("s.{}", "a".repeat(24));
        assert_redacted_persist(
            &s,
            &format!("see {secret} end"),
            "vault-service-token",
            &secret,
        );
        assert_not_redacted_persist(&s, "This is a sentence.");
    }

    #[test]
    fn vault_batch_token() {
        let s = fresh();
        let secret = format!("hvb.{}", "a".repeat(140));
        assert_redacted_persist(&s, &format!("see {secret} "), "vault-batch-token", &secret);
        assert_not_redacted_persist(&s, "see hvb.short ");
    }

    #[test]
    fn twilio_api_key() {
        let s = fresh();
        let secret = format!("SK{}", "a".repeat(32));
        assert_redacted_persist(&s, &format!("see {secret} "), "twilio-api-key", &secret);
        assert_not_redacted_persist(&s, &format!("see {} ", secret.to_ascii_lowercase()));
    }

    #[test]
    fn sendgrid_api_token() {
        let s = fresh();
        let secret = format!("SG.{}", "a".repeat(66));
        assert_redacted_persist(&s, &format!("see {secret} "), "sendgrid-api-token", &secret);
        assert_not_redacted_persist(&s, "see SG.short ");
    }

    #[test]
    fn mailgun_private_api_token() {
        let s = fresh();
        let value = format!("key-{}", "a".repeat(32));
        assert_redacted_persist(
            &s,
            &format!("mailgun_key=\"{value}\""),
            "mailgun-private-api-token",
            &value,
        );
        assert_not_redacted_persist(&s, "mailgun is an email delivery service");
    }

    #[test]
    fn square_access_token() {
        let s = fresh();
        let secret = format!("sq0atp-{}", "a".repeat(25));
        assert_redacted_persist(
            &s,
            &format!("see {secret} "),
            "square-access-token",
            &secret,
        );
        assert_not_redacted_persist(&s, "see sq0atp-short ");
    }

    #[test]
    fn shopify_access_token() {
        let s = fresh();
        let secret = format!("shpat_{}", "a".repeat(32));
        assert_redacted_persist(
            &s,
            &format!("see {secret} "),
            "shopify-access-token",
            &secret,
        );
        assert_not_redacted_persist(&s, "see shpat_short ");
    }

    #[test]
    fn new_relic_user_api_key() {
        let s = fresh();
        let value = format!("NRAK-{}", "a".repeat(27));
        assert_redacted_persist(
            &s,
            &format!("new_relic_key=\"{value}\""),
            "new-relic-user-api-key",
            &value,
        );
        assert_not_redacted_persist(&s, "new relic monitors application performance");
    }

    #[test]
    fn grafana_api_key() {
        let s = fresh();
        let secret = format!("eyJrIjoi{}", "a".repeat(80));
        assert_redacted_persist(&s, &format!("see {secret} "), "grafana-api-key", &secret);
        assert_not_redacted_persist(&s, "see eyJrIjoishort ");
    }

    #[test]
    fn planetscale_password() {
        let s = fresh();
        let secret = format!("pscale_pw_{}", "a".repeat(40));
        assert_redacted_persist(
            &s,
            &format!("see {secret} "),
            "planetscale-password",
            &secret,
        );
        assert_not_redacted_persist(&s, "see pscale_pw_short ");
    }

    #[test]
    fn generic_url_credential() {
        let s = fresh();
        let secret = "postgres://admin:hunters3cret@db.internal:5432/app".to_owned();
        assert_redacted_persist(&s, &secret, "generic-url-credential", "hunters3cret");
        assert_not_redacted_persist(&s, "https://example.com/path?a=b");
        assert_not_redacted_persist(
            &s,
            "see commit https://github.com/org/repo/commit/abcdef1234567890",
        );
    }

    // -- L2: every keyword in `generic-api-key`'s alternation, one mutation
    //    each -----------

    #[test]
    fn generic_api_key_every_alternation_keyword_is_covered() {
        let s = fresh();
        for keyword in [
            "access",
            "auth",
            "api",
            "credential",
            "creds",
            "key",
            "password",
            "passwd",
            "secret",
            "token",
        ] {
            let value = filler(24);
            let text = format!("{keyword}=\"{value}\"");
            let out = s.scrub_persist(&text);
            assert!(
                out.contains("[redacted:generic-api-key;"),
                "keyword `{keyword}` did not redact {text:?}, got {out:?}"
            );
        }
        // The specific mutation the review found unhandled: `password` with
        // its shorter, elided spelling, both live in the same alternation
        // (`passw(?:or)?d`).
        let value = filler(24);
        let text = format!("passwd=\"{value}\"");
        let out = s.scrub_persist(&text);
        assert!(out.contains("[redacted:generic-api-key;"));
    }

    // -- Model-facing path: registry only, pattern rules never run --------

    #[test]
    fn the_model_facing_path_never_applies_pattern_rules() {
        let s = fresh();
        // A real gitleaks-shaped secret, unregistered: the persist path
        // redacts it, the model-facing path must not touch it at all.
        let secret = format!("ghp_{}", "a".repeat(36));
        let text = format!("see {secret} in the diff");
        assert_eq!(&*s.scrub_model_facing(&text), text);
        assert_ne!(&*s.scrub_persist(&text), text);
    }

    #[test]
    fn the_model_facing_path_still_redacts_a_registered_exact_secret() {
        let s = fresh();
        let value = "kgraf-configured-provider-key-0000000";
        s.register_exact_secret("provider-api-key", value);
        let text = format!("echo {value}");
        let out = s.scrub_model_facing(&text);
        assert!(out.contains("[redacted:provider-api-key;"));
        assert!(!out.contains(value));
    }

    /// Regression test: every tracked workspace source file,
    /// scrubbed on the model-facing path, must come back byte-identical.
    /// This is the failure mode of the old, single-path design (50 of 651
    /// files changed) turned into a test
    /// that fails loudly if pattern rules are ever reintroduced to this path.
    #[test]
    fn model_facing_scrub_changes_no_tracked_workspace_source_file() {
        let s = fresh();
        // With an empty registry this test exercised
        // nothing, since the model path is the registry alone. Register
        // credential-shaped values in every kind kGraf registers. Each is
        // built at run time so the literal is in no tracked file, this one
        // included; a hit would mean the registry matched source text.
        for (kind, parts) in [
            (
                "provider-api-key",
                ["sk-ant-api03-", "Qw7Er9Ty2Ui4Op6As8Df0Gh1Jk3Lz5Xc"],
            ),
            (
                "provider-api-key",
                ["sk-proj-", "Mn4Bv6Cx8Za0Sd2Fg3Hj5Kl7Po9Iu1Yt"],
            ),
            (
                "mcp-header-env",
                ["Bearer ", "Rt5Yu7Io9Pa1Sd3Fg5Hj7Kl9Zx2Cv4Bn"],
            ),
            (
                "mcp-stdio-env",
                ["ghp_", "Lk8Jh6Gf4Ds2Ap0Oi9Uy7Tr5Ew3Qa1Zs2Xd"],
            ),
        ] {
            let value = parts.concat();
            assert_eq!(
                s.register_exact_secret(kind, &value),
                Registration::Registered
            );
        }
        let root = workspace_root();
        let files = tracked_text_files(&root);
        assert!(
            files.len() > 100,
            "the file list looks too small to be the real tree ({} files)",
            files.len()
        );
        let mut changed = Vec::new();
        for path in &files {
            let Ok(text) = std::fs::read_to_string(path) else {
                continue; // binary or non-UTF-8; not a text sink this path scrubs anyway
            };
            if s.scrub_model_facing(&text) != text {
                changed.push(
                    path.strip_prefix(&root)
                        .unwrap_or(path)
                        .display()
                        .to_string(),
                );
            }
        }
        assert!(
            changed.is_empty(),
            "the model-facing path must never change source text; it changed: {changed:#?}"
        );
    }

    /// The persist path's false-positive regression, from the security
    /// review's own list of real matches under the pre-fix design (every one
    /// a `generic-api-key` hit). With the entropy gate and allowlist restored,
    /// none of these lines redact.
    #[test]
    fn persist_path_false_positive_regression_list_stays_unchanged() {
        let s = fresh();
        for line in FALSE_POSITIVE_REGRESSION_LINES {
            assert_not_redacted_persist(&s, line);
        }
    }

    /// Lines the security review's probe reported as `generic-api-key`
    /// matches before the entropy gate and allowlist were restored. A
    /// representative sample across the shapes the review named (code
    /// assignments, SQL, test fixtures, comments), not the full list, kept
    /// small and readable; the full-tree regression above covers every
    /// tracked file, this one documents *why* by example.
    const FALSE_POSITIVE_REGRESSION_LINES: &[&str] = &[
        "stats.pinned_tokens = pinned_tokens;",
        "stats.code_tokens = code_spent;",
        "\"SELECT value FROM meta WHERE key = 'schema_version'\",",
        "schedule_id TEXT NOT NULL,",
        "env.settings.max_output_tokens = a.max_output_tokens;",
        "o.context_tokens = input_tokens;",
        "projection.tokens, projection.target_tokens, projection.source",
        "tokens: base_tokens + growth,",
        "api_version: \"2023-06-01\".to_owned(),",
        "self.ceiling_tokens = ceiling_tokens;",
        "let authority: AmbientAuthority = ambient_authority();",
        "consecutive_failures INTEGER NOT NULL DEFAULT 0,",
    ];

    fn workspace_root() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .expect("the workspace root")
    }

    /// Every `git`-tracked file under `root`, via `git ls-files`: the same
    /// source `kgraf-core`'s own brand-name workspace check uses, so this
    /// test and that one agree on what "every tracked file" means.
    fn tracked_text_files(root: &Path) -> Vec<std::path::PathBuf> {
        let output = std::process::Command::new("git")
            .args(["ls-files", "-z"])
            .current_dir(root)
            .output()
            .expect("git ls-files");
        assert!(output.status.success(), "git ls-files failed");
        String::from_utf8(output.stdout)
            .expect("git ls-files output is UTF-8 (paths are)")
            .split('\0')
            .filter(|s| !s.is_empty())
            .map(|s| root.join(s))
            .collect()
    }

    // -- The exact-secret registry (both paths) ---------------------------

    #[test]
    fn an_exact_secret_is_scrubbed_by_value_not_shape() {
        let s = fresh();
        let value = "not-shaped-like-anything-in-particular-1234";
        s.register_exact_secret("test-secret", value);
        let out = s
            .scrub_persist(&format!("the tool printed: {value}"))
            .into_owned();
        assert_eq!(
            out,
            "the tool printed: [redacted:test-secret; re-read the source for exact text]"
        );
    }

    #[test]
    fn an_exact_secret_shorter_than_the_minimum_is_never_registered() {
        let s = fresh();
        s.register_exact_secret("too-short", "1234567"); // 7 chars, below MIN_EXACT_SECRET_LEN
        assert_not_redacted_persist(&s, "value: 1234567 end");
    }

    #[test]
    fn every_occurrence_of_a_registered_secret_is_scrubbed() {
        let s = fresh();
        let value = "sk-configured-credential-value-000000";
        s.register_exact_secret("provider-api-key", value);
        let out = s
            .scrub_persist(&format!("first {value}, second {value}"))
            .into_owned();
        assert!(out.matches("[redacted:provider-api-key;").count() == 2);
        assert!(!out.contains(value));
    }

    #[test]
    fn an_exact_secret_and_a_pattern_rule_can_both_fire_in_one_call() {
        let s = fresh();
        let exact = "kgraf-configured-anthropic-key-000000";
        s.register_exact_secret("provider-api-key", exact);
        let pattern_secret = format!("ghp_{}", "a".repeat(36));
        let out = s
            .scrub_persist(&format!("env: {exact}, also {pattern_secret}"))
            .into_owned();
        assert!(out.contains("[redacted:provider-api-key;"));
        assert!(out.contains("[redacted:github-pat;"));
        assert!(!out.contains(exact));
        assert!(!out.contains(&pattern_secret));
    }

    #[test]
    fn a_benign_string_with_no_secret_shape_passes_through_borrowed() {
        let s = fresh();
        let text = "cargo test --workspace passed 1857 tests";
        match s.scrub_persist(text) {
            Cow::Borrowed(t) => assert_eq!(t, text),
            Cow::Owned(t) => panic!("nothing should have been redacted: {t:?}"),
        }
    }

    /// Any
    /// `[redacted:` in ordinary text used to switch scrubbing off up to the
    /// next `]`, so a string literal holding the marker prefix hid a real
    /// token on the lines after it.
    #[test]
    fn a_marker_prefix_in_source_text_does_not_hide_a_later_secret() {
        let s = fresh();
        let token = format!("ghp_{}", "Zq8vN2kLm4Rt6Yw1Xc3Bp5Hd7Jf9Gs0Ae2Ui");
        let source = format!(
            "let prefix = \"[redacted:\";\nlet token = \"{token}\";\nlet first = items[0];\n"
        );
        let out = s.scrub_persist(&source);
        assert!(!out.contains(&token), "{out}");
        assert!(out.contains("[redacted:github-pat;"), "{out}");
    }

    /// Only a complete marker in the exact form this crate writes is
    /// skipped; a lookalike with other wording is scanned like any text.
    #[test]
    fn only_an_exact_marker_this_crate_wrote_is_skipped() {
        let s = fresh();
        let value = "Kq7-rT2mZx9Lw4Pn8Vb3Hc6Jd1Fg5Sa0";
        s.register_exact_secret("test-secret", value);
        let lookalike = format!("[redacted:other-kind; re-read the source for exact text {value}]");
        let out = s.scrub_persist(&lookalike);
        assert!(!out.contains(value), "{out}");
        let exact = marker("other-kind");
        assert_eq!(
            &*s.scrub_persist(&exact),
            exact,
            "an exact marker is left alone"
        );
    }

    /// A low-entropy value such as LM Studio's placeholder key is not a
    /// credential and must not be registered; every "lm-studio" in a log
    /// would otherwise be redacted.
    #[test]
    fn a_low_entropy_value_is_not_registered() {
        let s = fresh();
        s.register_exact_secret("provider-api-key", "lm-studio");
        assert_not_redacted_persist(&s, "talking to lm-studio on localhost");
        assert_eq!(&*s.scrub_model_facing("lm-studio"), "lm-studio");
    }

    /// The scrub reports kind and count, and nothing else.
    #[test]
    fn a_counted_scrub_reports_each_kind_and_how_many() {
        let s = fresh();
        let exact = "Kq7-rT2mZx9Lw4Pn8Vb3Hc6Jd1Fg5Sa0";
        s.register_exact_secret("provider-api-key", exact);
        let pat = format!("ghp_{}", "Zq8vN2kLm4Rt6Yw1Xc3Bp5Hd7Jf9Gs0Ae2Ui");
        let text = format!("{exact} and {pat} and {pat}");
        let (out, counts) = s.scrub_persist_counted(&text);
        assert!(!out.contains(exact) && !out.contains(&pat), "{out}");
        let want: BTreeMap<String, u32> = [
            ("github-pat".to_owned(), 2),
            ("provider-api-key".to_owned(), 1),
        ]
        .into();
        assert_eq!(counts.counts(), &want);
        assert_eq!(counts.total(), 3);
        let (_, none) = s.scrub_persist_counted("nothing here");
        assert!(none.is_empty());
        // A second scrub of already-scrubbed text counts nothing again.
        let (_, again) = s.scrub_persist_counted(&out);
        assert!(again.is_empty(), "{again:?}");
        let (_, model) = s.scrub_model_facing_counted(&text);
        assert_eq!(model.total(), 1, "the model path counts only the registry");
    }

    /// A registration says what it did; the value never appears in it.
    #[test]
    fn a_registration_reports_its_outcome() {
        let s = fresh();
        assert_eq!(
            s.register_exact_secret("k", "short"),
            Registration::TooShort
        );
        assert_eq!(
            s.register_exact_secret("k", "lm-studio"),
            Registration::Placeholder
        );
        assert_eq!(
            s.register_exact_secret("k", "1212121212121212"),
            Registration::LowEntropy
        );
        let v = "Kq7-rT2mZx9Lw4Pn8Vb3Hc6Jd1Fg5Sa0";
        assert_eq!(s.register_exact_secret("k", v), Registration::Registered);
        assert_eq!(s.register_exact_secret("k", v), Registration::AlreadyKnown);
    }

    /// Intake stores the persist copy and keeps the model
    /// copy in memory; the model copy has no pattern marker, the stored copy
    /// has no pattern secret, and neither holds a registered secret.
    #[test]
    fn intake_keeps_a_registry_only_model_copy_beside_the_stored_copy() {
        let s = fresh();
        let exact = "Kq7-rT2mZx9Lw4Pn8Vb3Hc6Jd1Fg5Sa0";
        s.register_exact_secret("provider-api-key", exact);
        let pat = format!("ghp_{}", "Zq8vN2kLm4Rt6Yw1Xc3Bp5Hd7Jf9Gs0Ae2Ui");
        let raw = format!("use {pat} but never {exact}");
        let stored = s.intake(&raw);
        assert!(
            !stored.contains(&pat) && !stored.contains(exact),
            "{stored}"
        );
        let model = s.model_copy(&stored);
        assert!(
            model.contains(&pat),
            "the model sees the unregistered token"
        );
        assert!(!model.contains(exact), "never a registered secret");
        assert!(!model.contains("[redacted:github-pat"), "{model}");
        // Unknown text maps to itself.
        assert_eq!(&*s.model_copy("never taken in"), "never taken in");
        // Two originals with one shape each keep their own copy (see
        // `a_collided_slot_never_reopens_with_the_wrong_text`).
        let other = format!(
            "use ghp_{} but never {exact}",
            "Yz1aB2cD3eF4gH5iJ6kL7mN8oP9qR0sT1uV2"
        );
        let other_stored = s.intake(&other);
        assert_ne!(other_stored, stored);
        assert!(s.model_copy(&stored).contains(&pat));
        assert!(!s.model_copy(&other_stored).contains(&pat));
    }

    #[test]
    fn the_model_copy_table_is_bounded() {
        let mut copies = ModelCopies::default();
        for i in 0..MODEL_COPY_CAP + 10 {
            copies.remember(&format!("stored {i}"), &format!("model {i}"));
        }
        assert_eq!(copies.map.len(), MODEL_COPY_CAP);
        assert!(copies.get("stored 0").is_none(), "the oldest is dropped");
        assert_eq!(
            copies.get(&format!("stored {}", MODEL_COPY_CAP + 9)),
            Some(&*format!("model {}", MODEL_COPY_CAP + 9))
        );
    }

    // -- Marker forms, slots and caps --------------------------------------

    /// A marker whose kind is not a kind this crate writes is not a
    /// marker. A PAT, or a registered key, in the kind position is redacted.
    #[test]
    fn a_forged_marker_with_a_secret_as_its_kind_is_scrubbed() {
        let s = fresh();
        let registered = "Kq7-rT2mZx9Lw4Pn8Vb3Hc6Jd1Fg5Sa0";
        s.register_exact_secret("provider-api-key", registered);
        let pat = format!("ghp_{}", "Zq8vN2kLm4Rt6Yw1Xc3Bp5Hd7Jf9Gs0Ae2Ui");
        for secret in [pat.as_str(), registered] {
            let forged = format!("[redacted:{secret}; re-read the source for exact text]");
            for out in [s.scrub_persist(&forged), s.scrub_model_facing(&forged)] {
                if secret == pat && out == forged {
                    // The model path scrubs the registry only; a PAT there is
                    // not its job. It must not be hidden from the persist path.
                    continue;
                }
                assert!(!out.contains(secret), "{out}");
            }
            assert!(!s.scrub_persist(&forged).contains(secret));
        }
        // A registry kind is a kind: its marker is left alone.
        let own = marker("provider-api-key");
        assert_eq!(&*s.scrub_persist(&own), own);
    }

    /// A real short key is registered; a placeholder is refused, and
    /// the refusal is said, without the value.
    #[test]
    fn a_short_hex_key_registers_and_a_placeholder_is_refused_visibly() {
        let s = fresh();
        assert_eq!(
            s.register_exact_secret("provider-api-key", "9f86d081884c7d65"),
            Registration::Registered
        );
        assert_eq!(
            s.register_exact_secret("provider-api-key", "lm-studio"),
            Registration::Placeholder
        );
        let said = s.take_refusal_notices();
        assert_eq!(said.len(), 1, "{said:?}");
        assert!(said[0].contains("provider-api-key"), "{}", said[0]);
        assert!(
            !said[0].contains("lm-studio"),
            "never the value: {}",
            said[0]
        );
        // Said once per value.
        s.register_exact_secret("provider-api-key", "lm-studio");
        assert!(s.take_refusal_notices().is_empty());
    }

    /// A text equal to another text's stored form must not get that
    /// text's model copy.
    #[test]
    fn a_text_equal_to_another_stored_form_does_not_get_its_model_copy() {
        let s = fresh();
        let pat = format!("ghp_{}", "Zq8vN2kLm4Rt6Yw1Xc3Bp5Hd7Jf9Gs0Ae2Ui");
        let a = s.intake(&format!("use {pat}"));
        assert!(s.model_copy(&a).contains(&pat));
        // B is A's stored form, typed or copied by someone else.
        let b = s.intake(&a);
        let b_model = s.model_copy(&b).into_owned();
        assert!(!b_model.contains(&pat), "{b_model}");
    }

    /// Two texts with one scrubbed shape, then enough intakes to
    /// evict both: no stored form ever maps to the other text.
    #[test]
    fn a_collided_slot_never_reopens_with_the_wrong_text() {
        let s = fresh();
        let first = format!("ghp_{}", "Aa1Bb2Cc3Dd4Ee5Ff6Gg7Hh8Ii9Jj0Kk1Ll2");
        let second = format!("ghp_{}", "Mm3Nn4Oo5Pp6Qq7Rr8Ss9Tt0Uu1Vv2Ww3Xx4");
        let a = s.intake(&format!("key {first}"));
        let b = s.intake(&format!("key {second}"));
        assert_ne!(a, b, "each intake's stored form is its own");
        assert!(s.model_copy(&a).contains(&first) && !s.model_copy(&a).contains(&second));
        assert!(s.model_copy(&b).contains(&second) && !s.model_copy(&b).contains(&first));
        for i in 0..MODEL_COPY_CAP + 10 {
            s.intake(&format!("filler {i} {first}"));
        }
        let c = s.intake(&format!("key {second}"));
        assert!(!s.model_copy(&a).contains(&second));
        assert!(s.model_copy(&c).contains(&second));
    }

    /// The model-copy table is bounded in bytes too, and a caller can
    /// forget the copies it no longer needs.
    #[test]
    fn the_model_copy_table_is_bounded_in_bytes_and_can_forget() {
        let mut copies = ModelCopies::default();
        let big = "x".repeat(MODEL_COPY_MAX_BYTES / 4);
        for i in 0..8 {
            copies.remember(&format!("stored {i}"), &format!("{big}{i}"));
        }
        assert!(copies.bytes <= MODEL_COPY_MAX_BYTES, "{}", copies.bytes);
        assert!(copies.get("stored 0").is_none(), "oldest dropped for bytes");
        assert!(copies.get("stored 7").is_some());
        copies.forget("stored 7");
        assert!(copies.get("stored 7").is_none());
    }

    /// A bare 40-hex token as a git URL's userinfo.
    #[test]
    fn a_bare_hex_token_in_a_git_url_is_redacted() {
        let s = fresh();
        let token = "3f9a1c0e7b5d2a8f6c4e1b9d0a7c5e3f1b8d6a4c";
        let text = format!("git clone https://{token}@github.com/o/r.git");
        let out = s.scrub_persist(&text);
        assert!(!out.contains(token), "{out}");
        assert!(out.contains("[redacted:git-url-token;"), "{out}");
        // A commit hash in a URL path is not userinfo.
        assert_not_redacted_persist(&s, &format!("https://github.com/o/r/commit/{token}"));
    }

    /// Decision 0084: a checkout-root `.env`'s key values are scrubbed from
    /// model input; its ordinary settings are neither registered nor
    /// reported. Fake values only.
    #[test]
    fn dotenv_values_are_registered_quietly_and_scrubbed() {
        let s = fresh();
        let quoted = "gH7kQ2mZ9xR4vT1pL8sN3wB6";
        let exported = "Zr5Yq1Lm8Nc3Xv7Bt2Kp9Wd4";
        let bare = "sk-FAKEVENDORKEY0123456789abcdefFAKE";
        let text = format!(
            "# vendor keys\n\
             OPENAI_API_KEY=\"{quoted}\"\n\
             export GH_TOKEN='{exported}'\n\
             VENDOR_KEY={bare} # the main one\n\
             PORT=8080\n\
             MODE=development\n\
             not a pair\n"
        );
        assert_eq!(s.register_dotenv(&text), 3);
        assert!(s.take_refusal_notices().is_empty(), "ordinary values said");
        let shown = format!("build log: {quoted} {exported} {bare} on port 8080 in development");
        let out = s.scrub_model_facing(&shown);
        for secret in [quoted, exported, bare] {
            assert!(!out.contains(secret), "{out}");
        }
        assert!(out.contains("8080") && out.contains("development"), "{out}");
        // Registering the same file again adds nothing.
        assert_eq!(s.register_dotenv(&text), 0);
    }

    /// [`name_is_credential_shaped`] matches whole `_`/`-`-separated words,
    /// not a raw substring: `PATH` and `AUTHOR` must survive even though
    /// `path` contains `pat` and `author` contains `auth`.
    #[test]
    fn name_is_credential_shaped_is_word_based_not_substring() {
        for name in ["GITLAB_PAT", "BASIC_AUTH", "DB_CREDENTIAL", "API_KEY"] {
            assert!(name_is_credential_shaped(name), "{name} not matched");
        }
        for name in ["PATH", "DB_PATH", "AUTHOR_NAME", "PATCH_NOTES"] {
            assert!(!name_is_credential_shaped(name), "{name} matched");
        }
    }

    /// A dotenv value whose *name* does not look like a credential, and
    /// whose value does not match any pattern rule either, is never
    /// registered and so is never redacted from unrelated text that happens
    /// to contain the same bytes. Values chosen so none is a placeholder,
    /// too short, or shaped like one of the pattern rules.
    #[test]
    fn ordinary_dotenv_values_are_never_registered() {
        let s = fresh();
        let text = "TARGET=aarch64-apple-darwin\n\
                     MODEL_ID=claude-opus-5-5\n\
                     BIND_ADDR=127.0.0.1\n\
                     DATABASE_URL=localhost:5432\n\
                     RUST_LOG=debug,hyper=info\n\
                     DB_PATH=./data/db.sqlite\n\
                     RUST_TOOLCHAIN=nightly-2026-09-01\n";
        assert_eq!(s.register_dotenv(text), 0, "an ordinary value was kept");
        for value in [
            "aarch64-apple-darwin",
            "claude-opus-5-5",
            "127.0.0.1",
            "localhost:5432",
            "debug,hyper=info",
            "./data/db.sqlite",
            "nightly-2026-09-01",
        ] {
            let shown = format!("build log: {value} did the thing");
            assert!(
                !s.scrub_model_facing(&shown).contains("redacted"),
                "{value} was redacted though its name is ordinary"
            );
        }
    }

    /// A dotenv value is kept when its *name* is credential-shaped, even
    /// spelled with one of the less obvious needles (`credential`, `auth`,
    /// `pat`), and even when its value alone would not otherwise match a
    /// pattern rule.
    #[test]
    fn credential_shaped_dotenv_names_are_kept() {
        for (name, value) in [
            ("DB_CREDENTIAL", "N7qZ2wLp9RxK4mBt6Vc3Yd8S"),
            ("BASIC_AUTH", "T5vB8nQ1zXm4Kp7RdW2cLf9Y"),
            ("GITLAB_PAT", "Q3rK6wYm9BxL2pNt5Vd8Zc4S"),
        ] {
            let s = fresh();
            let text = format!("{name}={value}\n");
            assert_eq!(s.register_dotenv(&text), 1, "{name} was not kept");
            let shown = format!("build log: {value} appeared");
            assert!(
                s.scrub_model_facing(&shown).contains("redacted"),
                "{name}: {value} was not redacted"
            );
        }
    }

    /// A value that matches a pattern rule on its own is registered even
    /// under an ordinary-looking name ("or when a gitleaks pattern
    /// rule matches the value").
    #[test]
    fn a_pattern_shaped_value_is_kept_under_an_ordinary_name() {
        let s = fresh();
        let openai_key = "sk-FAKE01234567890abcdeT3BlbkFJFAKE01234567890abcde";
        let text = format!("SOME_SETTING={openai_key}\n");
        assert_eq!(s.register_dotenv(&text), 1, "pattern-shaped value dropped");
        let shown = format!("build log: {openai_key} appeared");
        assert!(s.scrub_model_facing(&shown).contains("redacted"), "{shown}");
    }

    /// Task 3: a quoted value keeps only the text up to its matching closing
    /// quote; a trailing comment, quoted-looking or not, is discarded either
    /// way. `dotenv_pairs` is private, so this is exercised through
    /// `register_dotenv`'s side effect on the model-facing scrub.
    #[test]
    fn a_quoted_dotenv_value_stops_at_its_matching_quote() {
        let s = fresh();
        let value = "N9pQ2xRw7YmB4Kd6Tf3Lc8Zv";
        let text = format!("A_KEY=\"{value}\" # comment\nB_TOKEN='{value}'#c\n");
        assert_eq!(s.register_dotenv(&text), 1, "the two lines share one value");
        let shown = format!("seen: {value}");
        let out = s.scrub_model_facing(&shown);
        assert!(!out.contains(value), "{out}");
        // The old, buggy parse would have registered the quote marks and the
        // comment as part of the value, so this text would not have matched.
        assert!(out.contains("redacted"), "{out}");
    }

    // -- Ordinary values, overlaps and caps -------------------------------

    /// A: ordinary configuration values are not credentials. Registered, each
    /// would be redacted from model input too (the class of false positive `generic-api-key` had). Each refusal
    /// is said.
    #[test]
    fn ordinary_values_are_refused_and_said() {
        let s = fresh();
        let ordinary = [
            "Microsoft",
            "Production",
            "Kerberos",
            "2026-09-27",
            "2026-09-27T10:00:00Z",
            "us-east-1a",
            "europe-west4",
            "12345678",
            "/usr/local/share/kgraf",
            "C:\\Users\\me\\kgraf",
            "https://login.microsoftonline.com/common",
        ];
        for value in ordinary {
            assert_ne!(
                s.register_exact_secret("mcp-stdio-env", value),
                Registration::Registered,
                "{value}"
            );
        }
        assert_eq!(s.take_refusal_notices().len(), ordinary.len());
        let text = "Sign in with Microsoft in us-east-1a on 2026-09-27";
        assert_eq!(&*s.scrub_model_facing(text), text);
        // Still keys: digits over 15, a URL carrying userinfo is left to the
        // URL rules, a real token.
        for key in [
            "1234567890123456",
            "9f86d081884c7d65",
            "Kq7-rT2mZx9Lw4Pn8Vb3Hc6Jd1Fg5Sa0",
        ] {
            assert_eq!(
                s.register_exact_secret("k", key),
                Registration::Registered,
                "{key}"
            );
        }
    }

    /// B, the reviewer's test: a registered secret that starts inside a
    /// pattern match must not leak its tail. Overlapping spans are merged.
    #[test]
    fn an_overlapping_registered_secret_leaks_no_tail() {
        let s = fresh();
        s.register_exact_secret("provider-api-key", "QRSTUVWX-abc123xyz9");
        let text = "key AKIAABCDEFGHQRSTUVWX-abc123xyz9 end";
        let out = s.scrub_persist(text);
        assert!(!out.contains("abc123xyz9"), "{out}");
        assert!(!out.contains("QRSTUVWX"), "{out}");
        assert!(out.ends_with(" end"), "{out}");
        let model = s.scrub_model_facing(text);
        assert!(!model.contains("abc123xyz9"), "{model}");
    }

    /// C: the marker scan is linear. 1 MB of openers with no `;` used to take
    /// over a second in release.
    #[test]
    fn a_megabyte_of_marker_openers_scans_fast() {
        let s = fresh();
        s.register_exact_secret("provider-api-key", "Kq7-rT2mZx9Lw4Pn8Vb3Hc6Jd1Fg5Sa0");
        let text = format!(
            "{}Kq7-rT2mZx9Lw4Pn8Vb3Hc6Jd1Fg5Sa0",
            "[redacted:".repeat(100_000)
        );
        let bound = if cfg!(debug_assertions) {
            std::time::Duration::from_millis(1_000)
        } else {
            std::time::Duration::from_millis(50)
        };
        let start = std::time::Instant::now();
        let out = s.scrub_persist(&text);
        let took = start.elapsed();
        assert!(!out.contains("Kq7-rT2mZx9Lw4Pn8Vb3Hc6Jd1Fg5Sa0"));
        assert!(took < bound, "took {took:?}, bound {bound:?}");
    }

    #[test]
    fn the_process_wide_global_scrubber_is_reachable_and_stable() {
        let a = global() as *const Scrubber;
        let b = global() as *const Scrubber;
        assert_eq!(a, b, "global() must return the same instance every call");
    }
}
