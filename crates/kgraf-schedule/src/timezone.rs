//! IANA time zone parsing.
//!
//! Decision 0052: "An unknown timezone is refused when the schedule is created."
//! Open WebUI's `recurrence.py:20-33` falls back to server time with a log line on a
//! bad zone string; that silent substitution is exactly the defect class the fail-loud rule
//! forbids, and the schedule would then fire at the wrong hour with no error to show for it. This
//! module makes that substitution structurally impossible: there is no code path here that returns
//! a `Tz` without either parsing the caller's string or raising.

use std::str::FromStr;

use chrono_tz::Tz;

use crate::error::ScheduleError;

/// Parse an IANA zone name (e.g. `"America/New_York"`, `"Europe/Stockholm"`, `"UTC"`).
///
/// An empty string, a misspelled zone, or a bare UTC offset (`"+02:00"`, which is not
/// an IANA zone and does not carry DST rules) is an error. There is no fallback zone.
pub fn parse_time_zone(zone: &str) -> Result<Tz, ScheduleError> {
    if zone.trim().is_empty() {
        return Err(ScheduleError::InvalidTimeZone {
            zone: zone.to_string(),
        });
    }
    Tz::from_str(zone).map_err(|_| ScheduleError::InvalidTimeZone {
        zone: zone.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_known_iana_zones() {
        assert_eq!(parse_time_zone("UTC").unwrap(), chrono_tz::UTC);
        assert_eq!(
            parse_time_zone("America/New_York").unwrap(),
            chrono_tz::America::New_York
        );
        assert_eq!(
            parse_time_zone("Europe/Stockholm").unwrap(),
            chrono_tz::Europe::Stockholm
        );
    }

    #[test]
    fn refuses_unknown_zone_without_falling_back() {
        let err = parse_time_zone("Mars/Olympus_Mons").unwrap_err();
        assert!(matches!(err, ScheduleError::InvalidTimeZone { .. }));
    }

    #[test]
    fn refuses_empty_zone() {
        let err = parse_time_zone("").unwrap_err();
        assert!(matches!(err, ScheduleError::InvalidTimeZone { .. }));
    }

    #[test]
    fn refuses_a_bare_utc_offset_because_it_is_not_an_iana_zone() {
        // An offset has no DST rule and is exactly the kind of "close enough"
        // substitution the fail-loud rule forbids; it must not silently parse as a zone.
        let err = parse_time_zone("+02:00").unwrap_err();
        assert!(matches!(err, ScheduleError::InvalidTimeZone { .. }));
    }

    /// Pins the IANA tzdb version this crate's DST tests were written and verified
    /// against (see `lib.rs`'s crate doc). A `chrono-tz` upgrade that bumps this
    /// constant is not itself a bug, but it is a prompt to re-check `recurrence`'s DST
    /// tests (spring-forward gap, fall-back ambiguity) against the new tzdb release
    /// before trusting them again: a tzdb release can, and periodically does, change a
    /// specific jurisdiction's transition date.
    #[test]
    fn the_bundled_tzdb_version_is_recorded_here() {
        assert_eq!(
            chrono_tz::IANA_TZDB_VERSION,
            "2025b",
            "chrono-tz's bundled IANA tzdb version changed; re-verify the DST tests in \
             the `recurrence` module against the new release, then update this constant \
             and the note in lib.rs"
        );
    }
}
