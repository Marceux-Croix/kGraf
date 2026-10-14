//! Rate-limit header capture (decision 0067).
//!
//! **Why this is not a field on [`crate::message::Response`].** The first draft interface put `pub limits: Option<ObservedLimits>` on
//! `Response`. `Response` is constructed by literal in around twenty places
//! across the workspace, several of them in `kgraf-runtime` (its own tests
//! and benches) and other units' owned files, which this unit's own scope
//! ("must not touch: ... `kgraf-runtime`") forbids editing, and which are
//! not this unit's files to edit even where they are not forbidden outright.
//! Adding a field there would have either broken every one of those call
//! sites' compilation or forced a `Default` derive and struct-update-syntax
//! rewrite of all of them, none of which belongs in this change. This module's
//! [`record`]/[`get`] pair gives the three adapters
//! a place to put what they parsed without touching
//! `Response` at all: reported here as a deviation from the draft interface,
//! not a silent substitution.
//!
//! **Keyed by base URL** (design: "keyed by base URL and credential
//! variable"). An adapter knows its own configured `base_url` but not the
//! user-facing route name a `RouteLimiter` is bound to (that mapping is
//! `kgraf-host::config`'s, one layer up); keying by base URL is what every
//! adapter can do unassisted, and two routes that share a base URL (a second
//! project's host pointed at the same vendor) legitimately share the same
//! vendor-reported limit anyway. The design's compound key, for the
//! two-credentials-one-base-URL case, was never called by a caller through
//! this module (dead code, removed); a future
//! caller with that case adds a compound key builder back when one exists to
//! call it.
//!
//! **The key is the URL the adapter holds after its settings resolve.** A catalog
//! URL with a placeholder (`{resource}`, `{project}`, `{region}`) never matches
//! what an adapter records, so the host derives the key with
//! `kgraf_fleet::limit_key`, which builds it from the same settings. An Azure
//! route adds the deployment ([`scoped_key`]), since Azure limits are per
//! deployment.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// What a vendor's rate-limit response headers said, the moment of one call.
/// Every field is `None` when that header was absent (never invented).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ObservedLimits {
    pub requests_limit: Option<u32>,
    pub tokens_limit: Option<u64>,
    pub requests_remaining: Option<u32>,
    /// Milliseconds until the window resets, from `now`. Vendors report this
    /// as a duration (OpenAI: `1m30s`-style) or an absolute instant
    /// (Anthropic: an RFC 3339 timestamp); both are normalized to "ms from
    /// now" here so a caller never needs to know which vendor's format it was.
    pub reset_ms: Option<i64>,
}

impl ObservedLimits {
    fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

fn registry() -> &'static Mutex<HashMap<String, ObservedLimits>> {
    static REGISTRY: OnceLock<Mutex<HashMap<String, ObservedLimits>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record what one call's headers said for `key` (a base URL). An all-`None`
/// [`ObservedLimits`] (nothing this module recognized in the response) is not
/// recorded, so `get` cannot be made to look like "no limit at all" by a
/// vendor that sent headers this crate does not yet parse. The record merges
/// field by field: a response that carries only some headers (a 429 with just
/// `retry-after`) updates those fields and leaves the rest as last seen,
/// instead of erasing the limits an earlier response reported.
pub fn record(key: &str, limits: ObservedLimits) {
    if limits.is_empty() {
        return;
    }
    let mut registry = registry().lock().unwrap_or_else(|p| p.into_inner());
    let merged = match registry.get(key) {
        Some(old) => ObservedLimits {
            requests_limit: limits.requests_limit.or(old.requests_limit),
            tokens_limit: limits.tokens_limit.or(old.tokens_limit),
            requests_remaining: limits.requests_remaining.or(old.requests_remaining),
            reset_ms: limits.reset_ms.or(old.reset_ms),
        },
        None => limits,
    };
    registry.insert(key.to_owned(), merged);
}

/// The most recently observed limits for `key`, if any call has reported one.
pub fn get(key: &str) -> Option<ObservedLimits> {
    registry()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(key)
        .copied()
}

/// Clears every recorded observation. `test-support` only: production code
/// never needs to forget what a vendor told it, and clearing between tests
/// is the only reason this exists (the registry is process-wide, so tests in
/// one binary would otherwise see each other's recordings).
#[cfg(any(test, feature = "test-support"))]
pub fn clear_for_test() {
    registry().lock().unwrap_or_else(|p| p.into_inner()).clear();
}

/// When a 429 (rate limited) response was last seen for `key` (a base URL),
/// in epoch milliseconds. Decision 0067
/// 2026-09-28: the per-machine capacity's learned rate
/// (`kgraf_capacity::LearnedRate`) halves a route's rate on a 429 and
/// recovers it over time; this is the one fact an adapter can report at the
/// moment a 429 arrives; the shell's re-measure thread (which owns the
/// `LearnedRate` state itself, one per route, since that state is a
/// decaying value over time and does not belong in a "what did the vendor
/// last say" registry like [`ObservedLimits`]) polls this to decide when to
/// call `on_429`. A separate, small registry from `record`/`get`: a 429 is a
/// discrete event, not a header value the next call simply overwrites.
fn registry_429() -> &'static Mutex<HashMap<String, i64>> {
    static REGISTRY: OnceLock<Mutex<HashMap<String, i64>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Records a 429 for `key` at `at_ms`.
pub fn record_429(key: &str, at_ms: i64) {
    registry_429()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(key.to_owned(), at_ms);
}

/// [`record_429`] with the current wall clock: what every production call
/// site uses; a test that needs a specific timestamp calls [`record_429`]
/// directly.
pub fn record_429_now(key: &str) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    record_429(key, now);
}

/// The last recorded 429 time for `key`, if any.
pub fn last_429(key: &str) -> Option<i64> {
    registry_429()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(key)
        .copied()
}

/// Clears every recorded 429. `test-support` only, same reasoning as
/// [`clear_for_test`].
#[cfg(any(test, feature = "test-support"))]
pub fn clear_429_for_test() {
    registry_429()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clear();
}

/// A minimal header reader: a closure over whatever header map an adapter
/// already has. A function, not a trait an adapter would `impl` for
/// `reqwest::header::HeaderMap`, because that type and this module's own
/// trait would both be foreign to the adapter crate (Rust's orphan rule
/// forbids implementing a foreign trait for a foreign type); a closure has
/// no such restriction and needs no dependency on `reqwest` or `http` here.
pub type HeaderGet<'a> = dyn Fn(&str) -> Option<String> + 'a;

fn get_u32(h: &HeaderGet, name: &str) -> Option<u32> {
    h(name)?.trim().parse().ok()
}

fn get_u64(h: &HeaderGet, name: &str) -> Option<u64> {
    h(name)?.trim().parse().ok()
}

/// OpenAI-style headers (also DeepSeek, Kimi/Moonshot's OpenAI-compatible
/// route and every other Chat Completions or Responses vendor this
/// workspace talks to): `x-ratelimit-limit-requests`,
/// `x-ratelimit-remaining-requests`, `x-ratelimit-limit-tokens`,
/// `x-ratelimit-reset-requests` (a duration like `1s`, `6m0s`, `250ms`).
pub fn parse_openai_style(h: &HeaderGet) -> ObservedLimits {
    ObservedLimits {
        requests_limit: get_u32(h, "x-ratelimit-limit-requests"),
        tokens_limit: get_u64(h, "x-ratelimit-limit-tokens"),
        requests_remaining: get_u32(h, "x-ratelimit-remaining-requests"),
        reset_ms: h("x-ratelimit-reset-requests")
            .as_deref()
            .and_then(parse_go_duration_ms),
    }
}

/// Azure OpenAI headers (Microsoft Learn, "Manage Azure OpenAI quota"):
/// the OpenAI names, limits that are per deployment, a reset given as a bare
/// number (the page shows `10` and `300` without a unit; read here as seconds),
/// and on a 429 `retry-after-ms`, or the standard `retry-after` in seconds, which
/// is the wait the service asks for and so wins over the window reset. The
/// value is recorded only; nothing waits on it.
pub fn parse_azure_style(h: &HeaderGet) -> ObservedLimits {
    let mut observed = parse_openai_style(h);
    observed.reset_ms = h("x-ratelimit-reset-requests")
        .as_deref()
        .and_then(parse_bare_seconds_ms)
        .or(observed.reset_ms);
    let retry_ms = h("retry-after-ms")
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v >= 0.0)
        .map(|v| v as i64)
        .or_else(|| h("retry-after").as_deref().and_then(parse_bare_seconds_ms));
    if retry_ms.is_some() {
        observed.reset_ms = retry_ms;
    }
    observed
}

/// A bare non-negative number of seconds, as milliseconds. Anything else is
/// `None`, including an HTTP-date `retry-after`.
fn parse_bare_seconds_ms(s: &str) -> Option<i64> {
    let seconds: f64 = s.trim().parse().ok()?;
    (seconds.is_finite() && seconds >= 0.0).then_some((seconds * 1_000.0) as i64)
}

/// The longest wait a response can ask for that is recorded: 24 hours. A
/// header naming more (a vendor bug, `1e30`) is read as this, so a recorded
/// value always fits a signed 64-bit column.
pub const MAX_RETRY_AFTER_MS: u64 = 24 * 60 * 60 * 1_000;

/// The wait a failed response asks for, in milliseconds, for the call's
/// ledger row and error: `retry-after-ms`, else `retry-after` as a bare
/// number of seconds, capped at [`MAX_RETRY_AFTER_MS`]. A negative, non-finite
/// or non-numeric value, and an HTTP-date `retry-after`, give `None`.
///
/// Recorded on the call only. It does not feed [`ObservedLimits`]: what the
/// per-route limit records read from these headers is unchanged by it.
pub fn retry_after_ms(h: &HeaderGet) -> Option<u64> {
    let ok = |v: f64| v.is_finite().then_some(v).filter(|v| *v >= 0.0);
    h("retry-after-ms")
        .and_then(|v| v.trim().parse::<f64>().ok())
        .and_then(ok)
        .or_else(|| {
            h("retry-after")
                .and_then(|v| v.trim().parse::<f64>().ok())
                .and_then(|secs| ok(secs * 1_000.0))
        })
        .map(|v| v.min(MAX_RETRY_AFTER_MS as f64) as u64)
}

/// The key one Azure deployment's limits are recorded under: the resource
/// origin and the deployment. Azure limits apply per deployment, so two
/// deployments on one resource must not overwrite each other.
pub fn scoped_key(base_url: &str, scope: &str) -> String {
    format!("{base_url}#{scope}")
}

/// Anthropic Messages headers: `anthropic-ratelimit-requests-limit`,
/// `anthropic-ratelimit-requests-remaining`, `anthropic-ratelimit-tokens-limit`,
/// `anthropic-ratelimit-requests-reset` (an RFC 3339 instant). Kimi's
/// Anthropic-compatible endpoint (no native Anthropic key in
/// this environment) is exercised through this same parser, since it speaks
/// the same header names on the same wire shape.
pub fn parse_anthropic_style(h: &HeaderGet) -> ObservedLimits {
    ObservedLimits {
        requests_limit: get_u32(h, "anthropic-ratelimit-requests-limit"),
        tokens_limit: get_u64(h, "anthropic-ratelimit-tokens-limit"),
        requests_remaining: get_u32(h, "anthropic-ratelimit-requests-remaining"),
        reset_ms: h("anthropic-ratelimit-requests-reset")
            .as_deref()
            .and_then(|v| parse_rfc3339_ms_from_now("anthropic-ratelimit-requests-reset", v)),
    }
}

/// Parses a Go-style duration string (`"1s"`, `"6m0s"`, `"250ms"`,
/// `"1h2m3s"`) into milliseconds. Returns `None` for anything else, rather
/// than guessing: an unrecognized reset format is better reported as absent
/// than silently wrong (fail loud).
fn parse_go_duration_ms(s: &str) -> Option<i64> {
    let mut total_ms: i64 = 0;
    let mut num = String::new();
    let mut any_unit = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() || c == '.' {
            num.push(c);
            continue;
        }
        // Unit: `ms`, or a single letter (`s`, `m`, `h`).
        let unit = if c == 'm' && chars.peek() == Some(&'s') {
            chars.next();
            "ms"
        } else {
            match c {
                's' => "s",
                'm' => "m",
                'h' => "h",
                _ => return None,
            }
        };
        let value: f64 = num.parse().ok()?;
        num.clear();
        any_unit = true;
        total_ms += match unit {
            "ms" => value,
            "s" => value * 1_000.0,
            "m" => value * 60_000.0,
            "h" => value * 3_600_000.0,
            _ => return None,
        } as i64;
    }
    if !num.is_empty() || !any_unit {
        return None;
    }
    Some(total_ms)
}

/// Parses an RFC 3339 timestamp and returns the milliseconds from `now` until
/// it.
///
/// The header is named as documented for Anthropic (`.../.../Z`), but RFC
/// 3339 also allows a numeric zone offset (`+00:00`, `-05:00`) instead of
/// `Z`, and nothing here guarantees every vendor speaking this header format
/// always emits Zulu time. A previous hand-rolled parser required the
/// trailing `Z` and returned `None` for anything else, which would have
/// silently disabled backoff timing against a vendor or proxy that emitted a
/// numeric offset instead, the header still contains a well-formed instant,
/// just not the one substring this parser looked for. `chrono::DateTime`'s
/// own RFC 3339 parser handles both forms correctly, and `chrono` is already
/// resolved in this workspace at this version, so it replaces the hand-rolled
/// date math rather than sitting beside it.
///
/// A timestamp in the past yields `Some` with a negative or zero value (the
/// window already reset), never `None`: only a genuinely unparsable string is
/// `None`, and that case is traced with the header name so a vendor changing
/// its reset format is visible in a debug trace rather than only showing up
/// as backoff quietly stopping.
fn parse_rfc3339_ms_from_now(header: &str, s: &str) -> Option<i64> {
    let parsed = match chrono::DateTime::parse_from_rfc3339(s.trim()) {
        Ok(instant) => instant,
        Err(error) => {
            tracing::trace!(
                header,
                value = s,
                %error,
                "rate-limit reset header is not a parsable RFC 3339 instant"
            );
            return None;
        }
    };
    let target_ms = parsed.timestamp_millis();
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_millis() as i64;
    Some(target_ms - now_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.to_string())
        }
    }

    fn owned_map(pairs: Vec<(&'static str, String)>) -> impl Fn(&str) -> Option<String> {
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.clone())
        }
    }

    #[test]
    fn openai_style_headers_parse_into_observed_limits() {
        let h = map(&[
            ("x-ratelimit-limit-requests", "500"),
            ("x-ratelimit-remaining-requests", "499"),
            ("x-ratelimit-limit-tokens", "200000"),
            ("x-ratelimit-reset-requests", "6m0s"),
        ]);
        let limits = parse_openai_style(&h);
        assert_eq!(limits.requests_limit, Some(500));
        assert_eq!(limits.requests_remaining, Some(499));
        assert_eq!(limits.tokens_limit, Some(200_000));
        assert_eq!(limits.reset_ms, Some(360_000));
    }

    #[test]
    fn azure_headers_read_a_bare_reset_and_the_retry_wait() {
        let plain = parse_azure_style(&map(&[
            ("x-ratelimit-limit-requests", "60"),
            ("x-ratelimit-limit-tokens", "150000"),
            ("x-ratelimit-remaining-requests", "59"),
            ("x-ratelimit-reset-requests", "10"),
        ]));
        assert_eq!(plain.requests_limit, Some(60));
        assert_eq!(plain.tokens_limit, Some(150_000));
        assert_eq!(plain.reset_ms, Some(10_000));

        let throttled = parse_azure_style(&map(&[
            ("x-ratelimit-reset-requests", "10"),
            ("retry-after-ms", "2000"),
            ("retry-after", "7"),
        ]));
        assert_eq!(throttled.reset_ms, Some(2_000), "retry-after-ms wins");

        let seconds_only = parse_azure_style(&map(&[("retry-after", "7")]));
        assert_eq!(seconds_only.reset_ms, Some(7_000));

        let date = parse_azure_style(&map(&[("retry-after", "Wed, 21 Oct 2026 07:28:00 GMT")]));
        assert_eq!(date.reset_ms, None, "an HTTP date is not guessed at");
    }

    #[test]
    fn retry_after_is_read_for_the_call_and_never_changes_the_recorded_limits() {
        let throttled = map(&[
            ("x-ratelimit-reset-requests", "6m0s"),
            ("anthropic-ratelimit-requests-reset", "2999-01-01T00:00:00Z"),
            ("retry-after-ms", "1500"),
            ("retry-after", "9"),
        ]);
        assert_eq!(retry_after_ms(&throttled), Some(1_500));
        assert_eq!(retry_after_ms(&map(&[("retry-after", "7")])), Some(7_000));
        // The reset the limits record is what it was without the header.
        let without = map(&[
            ("x-ratelimit-reset-requests", "6m0s"),
            ("anthropic-ratelimit-requests-reset", "2999-01-01T00:00:00Z"),
        ]);
        assert_eq!(
            parse_openai_style(&throttled).reset_ms,
            parse_openai_style(&without).reset_ms
        );
        assert_eq!(parse_openai_style(&throttled).reset_ms, Some(360_000));
        // No absolute reset here: the reader turns a reset date into a delay
        // from now, so two reads a millisecond apart would differ.
        let anthropic = map(&[
            ("anthropic-ratelimit-requests-limit", "50"),
            ("anthropic-ratelimit-requests-remaining", "0"),
            ("retry-after-ms", "1500"),
            ("retry-after", "9"),
        ]);
        let anthropic_without = map(&[
            ("anthropic-ratelimit-requests-limit", "50"),
            ("anthropic-ratelimit-requests-remaining", "0"),
        ]);
        assert_eq!(
            parse_anthropic_style(&anthropic),
            parse_anthropic_style(&anthropic_without),
            "retry-after changes nothing the anthropic reader records"
        );
        assert_eq!(parse_anthropic_style(&anthropic).reset_ms, None);
        assert_eq!(
            parse_openai_style(&map(&[("retry-after", "7")])).reset_ms,
            None
        );
    }

    fn one(name: &str, value: &str) -> impl Fn(&str) -> Option<String> {
        let (name, value) = (name.to_owned(), value.to_owned());
        move |asked| (asked == name).then(|| value.clone())
    }

    #[test]
    fn a_hostile_retry_after_is_capped_or_refused() {
        for huge in ["1e30", "1e300", "18446744073709551616"] {
            let secs = retry_after_ms(&one("retry-after", huge));
            let ms = retry_after_ms(&one("retry-after-ms", huge));
            assert_eq!(secs, Some(MAX_RETRY_AFTER_MS), "{huge}");
            assert_eq!(ms, Some(MAX_RETRY_AFTER_MS), "{huge}");
            assert!(i64::try_from(secs.unwrap_or(0)).is_ok());
        }
        assert_eq!(
            retry_after_ms(&map(&[("retry-after", "86400")])),
            Some(86_400_000)
        );
        assert_eq!(
            retry_after_ms(&map(&[("retry-after", "86401")])),
            Some(MAX_RETRY_AFTER_MS)
        );
        for bad in [
            "-1",
            "-0.5",
            "NaN",
            "inf",
            "soon",
            "Wed, 21 Oct 2026 07:28:00 GMT",
            "",
        ] {
            assert_eq!(retry_after_ms(&one("retry-after", bad)), None, "{bad}");
            assert_eq!(retry_after_ms(&one("retry-after-ms", bad)), None, "{bad}");
        }
    }

    #[test]
    fn scoped_keys_keep_two_deployments_apart() {
        clear_for_test();
        let a = scoped_key("https://r.openai.azure.com", "chat-a");
        let b = scoped_key("https://r.openai.azure.com", "chat-b");
        assert_ne!(a, b);
        record(
            &a,
            ObservedLimits {
                requests_limit: Some(10),
                ..ObservedLimits::default()
            },
        );
        assert!(get(&a).is_some());
        assert!(get(&b).is_none());
    }

    #[test]
    fn go_duration_parses_mixed_units() {
        assert_eq!(parse_go_duration_ms("250ms"), Some(250));
        assert_eq!(parse_go_duration_ms("1s"), Some(1_000));
        assert_eq!(parse_go_duration_ms("1m2s"), Some(62_000));
        assert_eq!(parse_go_duration_ms("1h2m3s"), Some(3_723_000));
        assert_eq!(parse_go_duration_ms("nonsense"), None);
        assert_eq!(parse_go_duration_ms(""), None);
    }

    #[test]
    fn anthropic_style_headers_parse_a_future_reset_as_positive_ms() {
        let future = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_secs()
            + 3600;
        // A hand-formatted RFC 3339 instant an hour from now, at second
        // resolution (sub-second is not needed for this test).
        let stamp = format_unix_as_rfc3339(future);
        let h = owned_map(vec![
            ("anthropic-ratelimit-requests-limit", "50".to_string()),
            ("anthropic-ratelimit-requests-remaining", "49".to_string()),
            ("anthropic-ratelimit-tokens-limit", "40000".to_string()),
            ("anthropic-ratelimit-requests-reset", stamp),
        ]);
        let limits = parse_anthropic_style(&h);
        assert_eq!(limits.requests_limit, Some(50));
        assert_eq!(limits.requests_remaining, Some(49));
        assert_eq!(limits.tokens_limit, Some(40_000));
        let ms = limits.reset_ms.expect("parsed");
        // An hour from now, give or take a few seconds of test execution time.
        assert!((3_500_000..3_700_000).contains(&ms), "{ms}");
    }

    #[test]
    fn a_numeric_zone_offset_parses_the_same_instant_as_zulu() {
        // RFC 3339 permits a numeric offset instead of `Z`. A vendor or proxy
        // reusing this header format is not guaranteed to always emit Zulu
        // time, and a parser that requires the literal `Z` suffix would treat
        // an offset instant as unparsable, silently disabling backoff timing
        // against it.
        let future = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_secs()
            + 3600;
        let zulu = format_unix_as_rfc3339(future);
        let as_offset = zulu.replace('Z', "+00:00");
        let h = owned_map(vec![("anthropic-ratelimit-requests-reset", as_offset)]);
        let limits = parse_anthropic_style(&h);
        let ms = limits.reset_ms.expect("a numeric +00:00 offset must parse");
        assert!((3_500_000..3_700_000).contains(&ms), "{ms}");

        // A non-zero offset must be honored, not just accepted and then
        // misread as Zulu: 05:00 in a +05:00 zone is midnight UTC.
        let five_hours_offset = parse_rfc3339_ms_from_now(
            "anthropic-ratelimit-requests-reset",
            "1970-01-01T05:00:00+05:00",
        )
        .expect("offset instant parses");
        let zulu_midnight =
            parse_rfc3339_ms_from_now("anthropic-ratelimit-requests-reset", "1970-01-01T00:00:00Z")
                .expect("zulu instant parses");
        assert_eq!(five_hours_offset, zulu_midnight);
    }

    #[test]
    fn an_unparsable_anthropic_reset_is_none_not_a_guess() {
        let h = owned_map(vec![(
            "anthropic-ratelimit-requests-reset",
            "not-an-instant".to_string(),
        )]);
        assert_eq!(parse_anthropic_style(&h).reset_ms, None);
    }

    /// A tiny, test-only inverse of `parse_rfc3339_ms_from_now`'s date math,
    /// kept hand-rolled (rather than switched to `chrono` alongside the
    /// parser) so this test formats the stamp by a path independent of the
    /// one under test.
    fn format_unix_as_rfc3339(unix_secs: u64) -> String {
        let days = unix_secs as i64 / 86_400;
        let rem = unix_secs as i64 % 86_400;
        let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
        let (y, mo, d) = civil_from_days(days);
        format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
    }

    fn civil_from_days(z: i64) -> (i64, i64, i64) {
        let z = z + 719_468;
        let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        (if m <= 2 { y + 1 } else { y }, m, d)
    }

    #[test]
    fn an_unparsable_reset_is_none_not_a_guess() {
        let h = map(&[("x-ratelimit-reset-requests", "soon")]);
        assert_eq!(parse_openai_style(&h).reset_ms, None);
    }

    #[test]
    fn all_absent_headers_give_an_all_none_result_and_is_not_recorded() {
        let h = map(&[]);
        let limits = parse_openai_style(&h);
        assert_eq!(limits, ObservedLimits::default());
        record("http://example.test", limits);
        assert_eq!(get("http://example.test"), None);
    }

    #[test]
    fn record_and_get_round_trip_by_key() {
        clear_for_test();
        let limits = ObservedLimits {
            requests_limit: Some(10),
            ..ObservedLimits::default()
        };
        record("http://a.test", limits);
        assert_eq!(get("http://a.test"), Some(limits));
        assert_eq!(get("http://b.test"), None);
    }

    #[test]
    fn a_response_with_only_some_headers_keeps_the_limits_an_earlier_one_reported() {
        clear_for_test();
        let key = "http://merge.test";
        record(
            key,
            ObservedLimits {
                requests_limit: Some(60),
                tokens_limit: Some(1_000),
                requests_remaining: Some(5),
                reset_ms: Some(9_000),
            },
        );
        // A 429 that carries only the wait the service asks for.
        record(
            key,
            ObservedLimits {
                reset_ms: Some(2_000),
                ..ObservedLimits::default()
            },
        );
        let seen = get(key).expect("recorded");
        assert_eq!(seen.requests_limit, Some(60));
        assert_eq!(seen.tokens_limit, Some(1_000));
        assert_eq!(seen.requests_remaining, Some(5));
        assert_eq!(seen.reset_ms, Some(2_000));
    }

    #[test]
    fn record_429_and_last_429_round_trip_by_key() {
        clear_429_for_test();
        assert_eq!(last_429("http://c.test"), None);
        record_429("http://c.test", 12_345);
        assert_eq!(last_429("http://c.test"), Some(12_345));
        assert_eq!(
            last_429("http://d.test"),
            None,
            "a different key is unaffected"
        );
        // A later 429 overwrites, since only the most recent matters.
        record_429("http://c.test", 99_999);
        assert_eq!(last_429("http://c.test"), Some(99_999));
    }

    #[test]
    fn record_429_now_uses_the_real_wall_clock() {
        clear_429_for_test();
        let before = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_millis() as i64;
        record_429_now("http://e.test");
        let recorded = last_429("http://e.test").expect("recorded");
        assert!(
            recorded >= before,
            "{recorded} should be at or after {before}"
        );
    }
}
