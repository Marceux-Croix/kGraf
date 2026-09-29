//! The `kgraf` binary's log subscriber.
//!
//! `kgraf-mcp` names targets whose logs can carry a secret
//! ([`kgraf_mcp::oauth::RECOMMENDED_LOG_FILTER`]: an OAuth authorization code,
//! an MCP session id). A line is written only if **both** of two filters let it
//! through:
//!
//! - the user's filter, from [`LOG_ENV`], in `EnvFilter` syntax (default
//!   `error`), which decides everything the user may tune;
//! - the lock, a fixed `Targets` filter: everything at trace, except the
//!   secret-bearing targets at the levels `RECOMMENDED_LOG_FILTER` gives them.
//!
//! **Why two filters and not one ** The lock was first added as
//! directives inside the user's `EnvFilter`. Span directives
//! (`[transport_worker]=trace`, `rmcp[transport_worker]=trace`) enable every event
//! inside a span, whatever its target, and bypassed it: session ids and
//! authorization codes were written. The lock is now a separate filter, so no
//! user directive of any shape can re-enable those lines.
//!
//! A user directive that names a locked target (or a target inside one) has no
//! effect, and the binary says so on stderr. An invalid directive is an error that
//! names [`LOG_ENV`] and the directive (fail loud; no command runs with a filter
//! the user did not mean).
//!
//! What the locked targets no longer log (for example a failed MCP GET stream)
//! is recorded as attributed `open_stream` events in the ledger instead.

use kgraf_mcp::oauth::RECOMMENDED_LOG_FILTER;
use tracing_subscriber::filter::{Directive, EnvFilter, FilterExt, Targets};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::{Layer, fmt::MakeWriter};

/// The environment variable holding the user's filter, in `EnvFilter` syntax.
pub const LOG_ENV: &str = "KGRAF_LOG";

/// The filter when the user sets none: errors only, so normal output is quiet.
const DEFAULT_FILTER: &str = "error";

/// A filter that could not be built.
#[derive(Debug, thiserror::Error)]
pub enum LogFilterError {
    #[error("{LOG_ENV} has an invalid directive `{directive}`")]
    Invalid {
        directive: String,
        #[source]
        source: tracing_subscriber::filter::ParseError,
    },
    #[error("the built-in log filter `{0}` does not parse")]
    Builtin(String),
}

/// The user's filter, the lock, and the user directives that have no effect.
pub struct LogFilter {
    user: EnvFilter,
    lock: Targets,
    /// User directives that name a locked target, reported on stderr.
    pub ignored: Vec<String>,
    /// User directives that named a crate by its former name (`haustoria_*`,
    /// decision 0062), with the directive read in their place; reported on
    /// stderr, so the rename never silently turns a directive into a no-op.
    pub renamed: Vec<(String, String)>,
}

/// `directive` with a target under the former crate names (`haustoria_x`)
/// read as the current one (`kgraf_x`), or `None` when it names neither.
/// Covers a bare target too (`haustoria_mcp`, `haustoria_mcp[span]`, no
/// level), which `EnvFilter` reads as "this target at trace".
fn renamed_directive(directive: &str) -> Option<String> {
    let target = directive_target(directive).or_else(|| {
        let bare = directive.split('[').next().unwrap_or(directive).trim();
        (!bare.is_empty()).then_some(bare)
    })?;
    let rest = target.strip_prefix("haustoria_")?;
    Some(directive.replacen(target, &format!("kgraf_{rest}"), 1))
}

/// The target a directive names (`target[span]=level`), or `None` for a bare
/// level (`trace`) or a span-only directive (`[span]=trace`).
fn directive_target(directive: &str) -> Option<&str> {
    let (target, _) = directive.split_once('=')?;
    let target = target.split('[').next().unwrap_or(target).trim();
    (!target.is_empty()).then_some(target)
}

/// The targets the lock holds below trace.
fn locked_targets() -> impl Iterator<Item = &'static str> {
    RECOMMENDED_LOG_FILTER
        .split(',')
        .filter_map(directive_target)
}

/// The filters for `user` (the contents of [`LOG_ENV`], if set).
pub fn filter(user: Option<&str>) -> Result<LogFilter, LogFilterError> {
    let mut user_filter = EnvFilter::default();
    let mut ignored = Vec::new();
    let mut renamed = Vec::new();
    for original in user
        .unwrap_or(DEFAULT_FILTER)
        .split(',')
        .map(str::trim)
        .filter(|d| !d.is_empty())
    {
        let current = renamed_directive(original);
        if let Some(current) = &current {
            renamed.push((original.to_owned(), current.clone()));
        }
        let directive = current.as_deref().unwrap_or(original);
        let parsed: Directive = directive
            .parse()
            .map_err(|source| LogFilterError::Invalid {
                directive: directive.to_owned(),
                source,
            })?;
        let names_locked = directive_target(directive).is_some_and(|target| {
            locked_targets()
                .any(|locked| target == locked || target.starts_with(&format!("{locked}::")))
        });
        if names_locked {
            ignored.push(directive.to_owned());
        }
        user_filter = user_filter.add_directive(parsed);
    }
    let lock_spec = format!("trace,{RECOMMENDED_LOG_FILTER}");
    let lock: Targets = lock_spec
        .parse()
        .map_err(|_| LogFilterError::Builtin(lock_spec.clone()))?;
    Ok(LogFilter {
        user: user_filter,
        lock,
        ignored,
        renamed,
    })
}

/// A subscriber writing to `writer` through both filters.
pub fn subscriber<W>(
    filter: LogFilter,
    writer: W,
    ansi: bool,
) -> impl tracing::Subscriber + Send + Sync
where
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    let layer = tracing_subscriber::fmt::layer()
        .with_writer(writer)
        .with_ansi(ansi)
        .with_filter(filter.user.and(filter.lock));
    tracing_subscriber::registry().with(layer)
}

/// Install the subscriber for this process, writing to stderr.
pub fn init() -> Result<(), LogFilterError> {
    let user = kgraf_core::legacy::env_var(LOG_ENV);
    let filter = filter(user.as_deref())?;
    for (former, current) in &filter.renamed {
        eprintln!(
            "{LOG_ENV}: `{former}` names a crate by kGraf's former name; read as `{current}` \
             (decision 0062)"
        );
    }
    for directive in &filter.ignored {
        eprintln!(
            "{LOG_ENV}: `{directive}` has no effect: that target can log secrets and stays \
             filtered"
        );
    }
    // Fails only if a subscriber is already set (a test harness); keep that one.
    // Colors only on a terminal: a log piped to a file or a harness is plain text.
    let ansi = std::io::IsTerminal::is_terminal(&std::io::stderr());
    let _ = tracing::subscriber::set_global_default(subscriber(filter, std::io::stderr, ansi));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("lock").extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for Captured {
        type Writer = Captured;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Emit, inside an `rmcp` worker span as the verifier's probe does, one line
    /// on each locked target and one elsewhere, under `user`.
    fn logged(user: &str) -> (String, Vec<String>) {
        let filter = filter(Some(user)).expect("filter");
        let ignored = filter.ignored.clone();
        let captured = Captured::default();
        tracing::subscriber::with_default(subscriber(filter, captured.clone(), false), || {
            let span = tracing::trace_span!(target: "rmcp::transport::worker", "transport_worker", name = "x");
            let _entered = span.enter();
            tracing::info!(
                target: "rmcp::transport::streamable_http_client",
                session_id = "SESSION-LEAK-1",
                "delete session success"
            );
            tracing::debug!(target: "rmcp::transport::auth", "code=AUTHCODE-LEAK-2");
            tracing::trace!(target: "kgraf_mcp::http", "open line");
        });
        let text = String::from_utf8(captured.0.lock().expect("lock").clone()).expect("utf8");
        (text, ignored)
    }

    /// No user directive re-enables the locked targets: not a target
    /// directive, not a broader one, and not a span directive (the verifier's
    /// bypass: `[transport_worker]=trace`, `[]=trace`, `rmcp[transport_worker]=trace`).
    #[test]
    fn a_user_filter_cannot_re_enable_the_locked_targets() {
        for user in [
            "trace,rmcp=trace,rmcp::transport::streamable_http_client=trace,\
             rmcp::transport::auth::inner=debug",
            "[transport_worker]=trace",
            "[]=trace",
            "rmcp[transport_worker]=trace",
            "trace,[transport_worker]=trace,rmcp[transport_worker]=trace",
        ] {
            let (text, _) = logged(user);
            assert!(!text.contains("SESSION-LEAK-1"), "{user}: {text}");
            assert!(!text.contains("AUTHCODE-LEAK-2"), "{user}: {text}");
        }
        let (text, ignored) = logged(
            "trace,rmcp=trace,rmcp::transport::streamable_http_client=trace,\
             rmcp::transport::auth::inner=debug",
        );
        assert!(
            text.contains("open line"),
            "other targets follow the user: {text}"
        );
        assert_eq!(
            ignored,
            [
                "rmcp::transport::streamable_http_client=trace",
                "rmcp::transport::auth::inner=debug"
            ]
        );
    }

    #[test]
    fn the_default_filter_is_quiet_and_an_invalid_one_names_the_directive() {
        let (text, ignored) = logged(DEFAULT_FILTER);
        assert!(!text.contains("open line"), "{text}");
        assert!(ignored.is_empty());
        let Err(error) = filter(Some("info,rmcp=notalevel")) else {
            panic!("an invalid directive must be an error");
        };
        let shown = error.to_string();
        assert!(shown.contains(LOG_ENV), "{shown}");
        assert!(shown.contains("rmcp=notalevel"), "{shown}");
    }

    /// Decision 0062: a directive naming a crate by its former name enables
    /// the renamed crate's lines, and is reported; the lock still holds.
    #[test]
    fn a_directive_under_the_former_crate_name_reaches_the_renamed_crate() {
        let renamed = filter(Some("haustoria_mcp=trace,rmcp=trace"))
            .expect("filter")
            .renamed;
        assert_eq!(
            renamed,
            [(
                "haustoria_mcp=trace".to_owned(),
                "kgraf_mcp=trace".to_owned()
            )]
        );
        let (text, _) = logged("haustoria_mcp=trace");
        assert!(text.contains("open line"), "{text}");
        let (text, _) = logged("haustoria_mcp[transport_worker]=trace");
        assert!(!text.contains("SESSION-LEAK-1"), "{text}");
        assert!(!text.contains("AUTHCODE-LEAK-2"), "{text}");
        // A bare target, no level, is renamed and reported too.
        let renamed = filter(Some("haustoria_mcp")).expect("filter").renamed;
        assert_eq!(
            renamed,
            [("haustoria_mcp".to_owned(), "kgraf_mcp".to_owned())]
        );
        let (text, _) = logged("haustoria_mcp");
        assert!(text.contains("open line"), "{text}");
        // A bare level is not a target.
        assert!(filter(Some("trace")).expect("filter").renamed.is_empty());
    }
}
