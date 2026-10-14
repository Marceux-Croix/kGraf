//! Land suggestions (decision 0100, section 5): kGraf offers to land when a
//! rule says it is a good moment, and never lands without the user's yes.
//!
//! The rules need no model call. Each needs a landable candidate: a commit of
//! `kgraf/<run>` not on the target whose root gate passed, that the stub rule
//! allows. Then any one of:
//!
//! 1. **Stopping point**: every contract verified, nothing in flight, and
//!    the root gate passed on the newest tip.
//! 2. **About to build on top**: the user starts new work on the files the
//!    unlanded commits changed. Its trigger needs routing of a change before
//!    it applies, which is built later: [`about_to_build_on_top`] never fires
//!    yet, and [`Suggester::evaluate`] does not call it.
//! 3. **The target moved**: the target gained a commit since the run started
//!    or last landed, while gated work waits.
//! 4. **Closing**: the session closes with gated work unlanded.
//!
//! Not nagging: at most one open suggestion. After "Not now", none until new
//! verified work has merged **and** [`DECLINE_QUIET_MS`] have passed; closing
//! ignores the interval. None while an interactive planner call runs or
//! within [`TYPING_QUIET_MS`] of typing. Declines are recorded in the run
//! record ([`LandBook::land_declined_at`], [`LandBook::land_decline_count`]),
//! so a restart keeps them.
//!
//! The permission mode never lands for the user: [`may_land`] is true only
//! for an explicit accept, in every mode. Landing is the user's decision; the
//! modes scale other questions only. A benchmark run raises no suggestion.

use crate::land_record::LandBook;
use kgraf_tools::PermissionMode;

/// After "Not now", no new suggestion for this long (and not before new
/// verified work): long enough not to ask again within one train of
/// thought, short enough to come back within a working session.
pub const DECLINE_QUIET_MS: i64 = 15 * 60 * 1000;

/// No suggestion within this long of the user typing.
pub const TYPING_QUIET_MS: i64 = 5_000;

/// Which rule raised a suggestion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SuggestRule {
    StoppingPoint,
    TargetMoved,
    Closing,
}

impl SuggestRule {
    /// Why now, for the card.
    pub fn reason(self) -> &'static str {
        match self {
            SuggestRule::StoppingPoint => {
                "every contract is verified and the root gate passed: a good moment to land"
            }
            SuggestRule::TargetMoved => {
                "the target branch gained commits while gated work waits; landing now keeps \
                 the merge small"
            }
            SuggestRule::Closing => "the session is closing with gated work not landed",
        }
    }
}

/// What the rules read. Built by the surface from the run record and the
/// repository; no model call.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SuggestFacts {
    /// A benchmark run: never a suggestion.
    pub benchmark: bool,
    /// A landable candidate with a passed root gate exists.
    pub landable: bool,
    /// Every contract verified, nothing in flight, and the root gate passed
    /// on the newest tip.
    pub stopping_point: bool,
    /// The target gained a commit since the run started or last landed.
    pub target_moved: bool,
    /// The session is closing.
    pub closing: bool,
    /// An interactive planner call is running.
    pub planner_call_running: bool,
    /// When the user last typed in the box (milliseconds since the epoch).
    pub last_typed_ms: Option<i64>,
    /// Verified contracts now.
    pub verified: usize,
    pub now_ms: i64,
    /// The branch a land would go into, and the commit it would land, with
    /// the count of its commits not on that branch: every card names them.
    pub target: Option<String>,
    pub candidate: Option<String>,
    pub commits: usize,
}

/// An open suggestion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Suggestion {
    pub rule: SuggestRule,
    pub raised_ms: i64,
}

/// The user's answer to a suggestion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SuggestAnswer {
    Land,
    NotNow,
}

/// Whether kGraf may land now. Only the user's explicit accept lands, in
/// every permission mode: Bypass asks nothing else, but landing moves the
/// user's branch, which is the user's decision. The mode is taken to make
/// that explicit at every call site.
pub fn may_land(_mode: PermissionMode, answer: Option<SuggestAnswer>) -> bool {
    matches!(answer, Some(SuggestAnswer::Land))
}

/// Rule 2 (about to build on top). Its trigger is a change routed before it
/// applies, which this build does not have, so it never fires; the other
/// rules do not call it.
pub fn about_to_build_on_top(_change_files: &[String], _unlanded_files: &[String]) -> bool {
    false
}

/// The one open suggestion, if any. The declines live in the run record.
#[derive(Clone, Debug, Default)]
pub struct Suggester {
    open: Option<Suggestion>,
}

impl Suggester {
    pub fn open(&self) -> Option<&Suggestion> {
        self.open.as_ref()
    }

    /// Raise a suggestion when a rule fires and nothing suppresses it.
    /// Returns the newly raised one; `None` when none fires or one is
    /// already open.
    pub fn evaluate(&mut self, book: &LandBook, f: &SuggestFacts) -> Option<Suggestion> {
        if self.open.is_some() || f.benchmark || !f.landable || f.planner_call_running {
            return None;
        }
        if f.last_typed_ms
            .is_some_and(|t| f.now_ms.saturating_sub(t) < TYPING_QUIET_MS)
        {
            return None;
        }
        let rule = if f.closing {
            SuggestRule::Closing
        } else if f.stopping_point {
            SuggestRule::StoppingPoint
        } else if f.target_moved {
            SuggestRule::TargetMoved
        } else {
            return None;
        };
        if let Some(declined) = book.land_declined_at {
            let new_work = book.verified_at_decline.is_none_or(|n| f.verified > n);
            let quiet = f.now_ms.saturating_sub(declined) >= DECLINE_QUIET_MS;
            let suppressed = if rule == SuggestRule::Closing {
                false
            } else {
                !(new_work && quiet)
            };
            if suppressed {
                return None;
            }
        }
        let s = Suggestion {
            rule,
            raised_ms: f.now_ms,
        };
        self.open = Some(s.clone());
        Some(s)
    }

    /// The user's answer to the open suggestion. "Not now" is recorded in
    /// `book` (the caller saves it). Returns whether to land now.
    pub fn answer(
        &mut self,
        book: &mut LandBook,
        mode: PermissionMode,
        answer: SuggestAnswer,
        now_ms: i64,
        verified: usize,
    ) -> bool {
        if self.open.take().is_none() {
            return false;
        }
        if answer == SuggestAnswer::NotNow {
            book.land_declined_at = Some(now_ms);
            book.land_decline_count += 1;
            book.verified_at_decline = Some(verified);
        }
        may_land(mode, Some(answer))
    }

    /// Close the open suggestion without an answer (it landed another way,
    /// or nothing is landable any more).
    pub fn withdraw(&mut self) {
        self.open = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(now: i64) -> SuggestFacts {
        SuggestFacts {
            landable: true,
            stopping_point: true,
            verified: 3,
            now_ms: now,
            ..SuggestFacts::default()
        }
    }

    #[test]
    fn each_computable_rule_fires_on_its_side_only() {
        let book = LandBook::default();
        let mut s = Suggester::default();
        assert_eq!(
            s.evaluate(&book, &facts(0)).map(|x| x.rule),
            Some(SuggestRule::StoppingPoint)
        );
        let mut s = Suggester::default();
        let moved = SuggestFacts {
            stopping_point: false,
            target_moved: true,
            ..facts(0)
        };
        assert_eq!(
            s.evaluate(&book, &moved).map(|x| x.rule),
            Some(SuggestRule::TargetMoved)
        );
        let mut s = Suggester::default();
        let closing = SuggestFacts {
            stopping_point: false,
            closing: true,
            ..facts(0)
        };
        assert_eq!(
            s.evaluate(&book, &closing).map(|x| x.rule),
            Some(SuggestRule::Closing)
        );
        let mut s = Suggester::default();
        let none = SuggestFacts {
            stopping_point: false,
            ..facts(0)
        };
        assert_eq!(s.evaluate(&book, &none), None, "no rule fired");
        let unlandable = SuggestFacts {
            landable: false,
            ..facts(0)
        };
        assert_eq!(s.evaluate(&book, &unlandable), None, "nothing landable");
    }

    #[test]
    fn about_to_build_on_top_never_fires_yet() {
        assert!(!about_to_build_on_top(&["a.rs".into()], &["a.rs".into()]));
    }

    #[test]
    fn only_one_suggestion_is_open() {
        let book = LandBook::default();
        let mut s = Suggester::default();
        assert!(s.evaluate(&book, &facts(0)).is_some());
        assert!(s.evaluate(&book, &facts(1)).is_none());
    }

    #[test]
    fn a_benchmark_run_raises_none() {
        let book = LandBook::default();
        let mut s = Suggester::default();
        for f in [
            facts(0),
            SuggestFacts {
                closing: true,
                ..facts(0)
            },
            SuggestFacts {
                target_moved: true,
                ..facts(0)
            },
        ] {
            let f = SuggestFacts {
                benchmark: true,
                ..f
            };
            assert_eq!(s.evaluate(&book, &f), None);
        }
    }

    #[test]
    fn never_during_a_planner_call_or_right_after_typing() {
        let book = LandBook::default();
        let mut s = Suggester::default();
        let busy = SuggestFacts {
            planner_call_running: true,
            ..facts(10_000)
        };
        assert_eq!(s.evaluate(&book, &busy), None);
        let typing = SuggestFacts {
            last_typed_ms: Some(6_000),
            ..facts(10_000)
        };
        assert_eq!(s.evaluate(&book, &typing), None);
        let quiet = SuggestFacts {
            last_typed_ms: Some(4_000),
            ..facts(10_000)
        };
        assert!(s.evaluate(&book, &quiet).is_some());
    }

    /// After "Not now": new work alone, the interval alone, and both.
    #[test]
    fn not_now_suppresses_until_new_work_and_the_interval() {
        let mut book = LandBook::default();
        let mut s = Suggester::default();
        s.evaluate(&book, &facts(0)).expect("raised");
        let land = s.answer(
            &mut book,
            PermissionMode::Bypass,
            SuggestAnswer::NotNow,
            0,
            3,
        );
        assert!(!land);
        assert_eq!(book.land_decline_count, 1);
        assert_eq!(book.land_declined_at, Some(0));
        let later = DECLINE_QUIET_MS;
        let work_only = SuggestFacts {
            verified: 4,
            ..facts(later - 1)
        };
        assert_eq!(s.evaluate(&book, &work_only), None, "new work alone");
        let time_only = facts(later);
        assert_eq!(s.evaluate(&book, &time_only), None, "the interval alone");
        let both = SuggestFacts {
            verified: 4,
            ..facts(later)
        };
        assert!(s.evaluate(&book, &both).is_some(), "both");
    }

    #[test]
    fn closing_ignores_the_interval() {
        let mut book = LandBook::default();
        let mut s = Suggester::default();
        s.evaluate(&book, &facts(0)).expect("raised");
        s.answer(
            &mut book,
            PermissionMode::Manual,
            SuggestAnswer::NotNow,
            0,
            3,
        );
        let closing = SuggestFacts {
            closing: true,
            ..facts(1)
        };
        assert_eq!(
            s.evaluate(&book, &closing).map(|x| x.rule),
            Some(SuggestRule::Closing)
        );
    }

    /// The decline survives a restart: it is in the record, not the
    /// suggester.
    #[test]
    fn the_decline_survives_a_restart() {
        let mut book = LandBook::default();
        let mut s = Suggester::default();
        s.evaluate(&book, &facts(0)).expect("raised");
        s.answer(&mut book, PermissionMode::Auto, SuggestAnswer::NotNow, 0, 3);
        let saved = serde_json::to_string(&book).unwrap();
        let book: LandBook = serde_json::from_str(&saved).unwrap();
        let mut fresh = Suggester::default();
        assert_eq!(fresh.evaluate(&book, &facts(60_000)), None);
        assert_eq!(book.land_decline_count, 1);
    }

    /// In every mode, nothing lands without an explicit accept: no answer
    /// and "Not now" never land; only "Land" does.
    #[test]
    fn no_mode_lands_without_an_explicit_accept() {
        for mode in PermissionMode::ALL {
            assert!(!may_land(mode, None), "{mode:?} with no answer");
            assert!(!may_land(mode, Some(SuggestAnswer::NotNow)), "{mode:?}");
            assert!(may_land(mode, Some(SuggestAnswer::Land)), "{mode:?}");
            let mut book = LandBook::default();
            let mut s = Suggester::default();
            assert!(
                !s.answer(&mut book, mode, SuggestAnswer::Land, 0, 0),
                "{mode:?}: no open suggestion, nothing to accept"
            );
            s.evaluate(&book, &facts(0)).expect("raised");
            assert!(s.answer(&mut book, mode, SuggestAnswer::Land, 0, 3));
        }
    }
}
