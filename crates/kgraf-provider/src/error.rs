//! Provider errors.
//!
//! Every variant here corresponds to a failure mode that was observed returning
//! HTTP 200 from a real provider. The point of naming them is the fail-loud rule: silent
//! degradation is a defect, so each one raises.

/// What can go wrong reaching a provider.
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    /// The adapter was asked for a model it does not serve.
    ///
    /// Checked **before** the request goes out. One provider silently remaps an
    /// unknown model name to its cheap model and returns 200, so a typo or a
    /// stale configuration is a correctness bug in a worker pool, not only a
    /// billing one.
    #[error("adapter `{adapter}` cannot serve model `{model}`; it serves: {available}")]
    ModelUnsupported {
        adapter: &'static str,
        model: String,
        available: String,
    },

    /// The response came back from a different model than was requested.
    ///
    /// Checked **after** the response arrives, because the pre-flight check can
    /// only catch what the adapter knows about. This is the assertion decision
    /// 0006 requires of every adapter.
    #[error("requested model `{requested}` but the response came from `{returned}`")]
    ModelSubstituted { requested: String, returned: String },

    /// A capability was requested that this provider does not honor.
    ///
    /// Recorded as not honored rather than assumed. Emitting cache breakpoints at
    /// a provider that drops them costs money silently and gains nothing, and one
    /// provider has no cache-control field in its schema at all.
    #[error("provider `{adapter}` does not honor `{capability}`")]
    CapabilityNotHonored {
        adapter: &'static str,
        capability: &'static str,
    },

    /// The request never reached the provider: the connection could not be made.
    ///
    /// Distinct from [`ProviderError::Transport`] because it is the only transport
    /// failure that provably billed nothing. A timeout or a reset after the request
    /// was written may have billed in full.
    #[error("request to `{adapter}` was never sent: {reason}")]
    Unsent {
        adapter: &'static str,
        reason: String,
    },

    /// Transport failure after the request may have reached the provider.
    #[error("transport failure talking to `{adapter}`: {reason}")]
    Transport {
        adapter: &'static str,
        reason: String,
    },

    /// The provider returned a non-success status.
    #[error("provider `{adapter}` returned HTTP {status}: {body}")]
    Http {
        adapter: &'static str,
        status: u16,
        body: String,
    },

    /// The response did not match the protocol's schema.
    #[error("cannot decode `{adapter}` response: {reason}")]
    Decode {
        adapter: &'static str,
        reason: String,
    },

    /// The provider sent no usage at all, so the call cannot be accounted for.
    ///
    /// Not defaulted to zero: a call with no usage is either a protocol change or
    /// a dropped stream, and both need to be seen.
    #[error("provider `{adapter}` returned no usage; the call cannot be attributed")]
    UsageMissing { adapter: &'static str },

    /// The stream ended before its terminal event, with only a **partial**
    /// usage figure observed.
    ///
    /// Distinct from [`Self::UsageMissing`]: some usage was seen (one vendor's
    /// `message_start` carries the input side before any output exists), but
    /// the response the transport handed back never reached completion, so
    /// that figure is a stated lower bound, not the call's final one. The
    /// caller settles the token as unreconciled with that partial figure,
    /// see [`crate::CallToken::leave_unreconciled_with`], rather than as
    /// `Counted`, which would silently understate spend.
    #[error(
        "stream from `{adapter}` ended before its terminal event; only partial usage was observed"
    )]
    StreamTruncated { adapter: &'static str },

    /// Usage arrived but could not be normalized.
    #[error("cannot normalize usage from `{adapter}`")]
    Usage {
        adapter: &'static str,
        #[source]
        source: kgraf_core::usage::UsageError,
    },

    /// The ledger refused the call.
    #[error("accounting failure")]
    Accounting(#[from] crate::accounting::LedgerError),

    /// The call was cancelled.
    #[error("call to `{adapter}` was cancelled")]
    Cancelled { adapter: &'static str },

    /// Credentials are absent. An error at configuration time, not an empty
    /// result at run time.
    #[error("no credential configured for `{adapter}`")]
    MissingCredential { adapter: &'static str },

    /// The route's configured rate limit would hold the call longer than its
    /// allowed wait. The request was never sent (`ratelimit.rs`).
    ///
    /// `ledger_error` carries a failure to close the call's ledger row. It is
    /// reported here rather than replacing the refusal: the refusal is what the
    /// caller must act on, and the row stays in flight, where `unreconciled`
    /// reports it.
    ///
    /// `lock_contended` distinguishes a genuine rate-limit refusal (this
    /// process's own quota is exhausted) from a shared-store check that ran
    /// out of its wait budget because *another process's connection held the
    /// SQLite write lock* (a stuck lock
    /// looks nothing like real rate limiting to whoever reads the notice, so
    /// it must say which one happened rather than reporting both the same
    /// way). `false` on every [`Backend::Local`](crate::ratelimit) refusal
    /// and on a shared refusal that came from the quota itself, not the lock.
    #[error(
        "route `{route}` ({adapter}) is rate limited: the next slot is {wait_ms} ms away, over the {max_wait_ms} ms allowed wait{}{}",
        if *lock_contended { " (another process is holding the shared rate-limit store's write lock, not the route's real quota)" } else { "" },
        ledger_error.as_ref().map(|e| format!("; the ledger row could not be closed: {e}")).unwrap_or_default()
    )]
    RateLimited {
        adapter: &'static str,
        route: String,
        wait_ms: u64,
        max_wait_ms: u64,
        lock_contended: bool,
        ledger_error: Option<String>,
    },

    /// The cross-process shared rate limit store could not be read or written
    /// (`crosslimit.rs`). The request was never sent: per the fail-loud rule this fails
    /// loud rather than silently limiting only within this process.
    ///
    /// `ledger_error` mirrors [`Self::RateLimited`]'s field: a
    /// failure to close the call's ledger row is reported here too, rather
    /// than dropped, so this variant's row landing in `unreconciled` is never
    /// a silent surprise.
    #[error(
        "route `{route}` ({adapter}) cannot be rate limited across processes: {detail}{}",
        ledger_error.as_ref().map(|e| format!("; the ledger row could not be closed: {e}")).unwrap_or_default()
    )]
    RateLimitStoreUnavailable {
        adapter: &'static str,
        route: String,
        detail: String,
        ledger_error: Option<String>,
    },
}

impl kgraf_diag::DiagCode for ProviderError {
    fn diag_code(&self) -> &'static str {
        match self {
            ProviderError::MissingCredential { .. } => "KG-0301",
            ProviderError::Http { .. } => "KG-0302",
            ProviderError::RateLimited { .. } => "KG-0303",
            ProviderError::ModelUnsupported { .. } => "KG-0304",
            ProviderError::UsageMissing { .. } => "KG-0305",
            _ => kgraf_diag::catch_all_for("kgraf-provider").code,
        }
    }

    fn diag_component(&self) -> &'static str {
        "kgraf-provider"
    }
}

/// Whether a call that ended in an error may have been billed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Billing {
    /// The provider provably did no billable work: the request never left, or it
    /// was rejected before generation.
    NotBilled,
    /// The request reached the provider and the outcome of billing is unknown.
    MayHaveBilled,
}

impl ProviderError {
    /// Whether the call that produced this error may have been billed.
    ///
    /// Conservative by construction. Only outcomes that prove no generation ran
    /// are [`Billing::NotBilled`]; everything else leaves the ledger row open,
    /// because a failed attempt the provider billed is spend, and closing it as
    /// failed would understate it silently (fail loud).
    ///
    /// A 4xx other than 408 is a rejection before generation. A 408, a 5xx, a
    /// transport error after the connection was made, an undecodable body and a
    /// substituted model all arrive after the provider may have generated.
    pub fn billing(&self) -> Billing {
        match self {
            ProviderError::ModelUnsupported { .. }
            | ProviderError::CapabilityNotHonored { .. }
            | ProviderError::MissingCredential { .. }
            | ProviderError::Unsent { .. }
            | ProviderError::RateLimited { .. }
            | ProviderError::RateLimitStoreUnavailable { .. } => Billing::NotBilled,
            ProviderError::Http { status, .. } if (400..500).contains(status) && *status != 408 => {
                Billing::NotBilled
            }
            _ => Billing::MayHaveBilled,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn http(status: u16) -> ProviderError {
        ProviderError::Http {
            adapter: "t",
            status,
            body: String::new(),
        }
    }

    /// Every `diag_code` branch resolves to a real glossary entry (renaming `MissingCredential`'s code to
    /// something not in `glossary::GLOSSARY` (or anything else `diag_code`
    /// can return) passed every other test in this crate before this one
    /// existed. One instance per branch of the `match` in `diag_code`,
    /// including the wildcard (`ModelSubstituted`, which is not named there):
    /// not literally every variant, since most of the unnamed ones share the
    /// same catch-all branch as `ModelSubstituted` and would prove nothing
    /// additional.
    #[test]
    fn every_diag_code_branch_names_a_real_glossary_entry() {
        use kgraf_diag::DiagCode;
        let instances: Vec<ProviderError> = vec![
            ProviderError::MissingCredential { adapter: "t" },
            http(500),
            ProviderError::RateLimited {
                adapter: "t",
                route: "t".into(),
                wait_ms: 0,
                max_wait_ms: 0,
                lock_contended: false,
                ledger_error: None,
            },
            ProviderError::ModelUnsupported {
                adapter: "t",
                model: "m".into(),
                available: String::new(),
            },
            ProviderError::UsageMissing { adapter: "t" },
            // Not named in `diag_code`'s match: exercises the catch-all arm.
            ProviderError::ModelSubstituted {
                requested: "a".into(),
                returned: "b".into(),
            },
        ];
        for e in instances {
            let code = e.diag_code();
            assert!(
                kgraf_diag::GLOSSARY.iter().any(|spec| spec.code == code),
                "{e:?} names code {code}, which is not in kgraf_diag::GLOSSARY"
            );
        }
    }

    #[test]
    fn only_outcomes_that_prove_no_generation_are_not_billed() {
        assert_eq!(http(400).billing(), Billing::NotBilled);
        assert_eq!(http(429).billing(), Billing::NotBilled);
        assert_eq!(http(408).billing(), Billing::MayHaveBilled);
        assert_eq!(http(500).billing(), Billing::MayHaveBilled);
        assert_eq!(http(529).billing(), Billing::MayHaveBilled);
        let unsent = ProviderError::Unsent {
            adapter: "t",
            reason: String::new(),
        };
        assert_eq!(unsent.billing(), Billing::NotBilled);
        let transport = ProviderError::Transport {
            adapter: "t",
            reason: String::new(),
        };
        assert_eq!(transport.billing(), Billing::MayHaveBilled);
        let decode = ProviderError::Decode {
            adapter: "t",
            reason: String::new(),
        };
        assert_eq!(decode.billing(), Billing::MayHaveBilled);
    }
}
