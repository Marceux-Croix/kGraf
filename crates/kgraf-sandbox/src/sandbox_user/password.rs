//! The sandbox users' passwords: long, random, never printed.
//!
//! Nobody types these passwords. Only kGraf's own process uses them, to
//! start a command as the sandbox user. So they are long (40 characters,
//! about 250 bits) and drawn from the system random source with rejection
//! sampling (no modulo bias). Each holds an uppercase letter, a lowercase
//! letter, a digit and a symbol, and no 3-character part of the account
//! names, so Windows' password complexity policy accepts them where it is
//! on. The symbols leave out whitespace, quotes, backslash and the
//! characters a command line or a file format would treat specially.
//!
//! [`Password`] clears its memory when dropped (`zeroize`) and its `Debug`
//! output is a placeholder; it has no `Display`.

use std::fmt;

use zeroize::Zeroizing;

/// Characters in a password.
pub const PASSWORD_LEN: usize = 40;

/// The symbol characters a password may hold.
pub const PASSWORD_SYMBOLS: &str = "!#%+-.:=?@^_~";

/// Attempts before generation gives up (a candidate fails the class or name
/// check with a probability far below one in a thousand, so this is reached
/// only with a broken random source).
const MAX_ATTEMPTS: usize = 64;

#[derive(Debug, thiserror::Error)]
pub enum PasswordError {
    #[error("the system random source failed: {0}")]
    Random(String),
    #[error(
        "no password met the rules after {MAX_ATTEMPTS} attempts; the random source is not random"
    )]
    Exhausted,
    #[error("a received password is not in the expected form")]
    Malformed,
}

/// A sandbox user's password.
pub struct Password(Zeroizing<String>);

impl Password {
    /// A new password from the system random source.
    pub fn generate(forbidden_parts: &[String]) -> Result<Self, PasswordError> {
        Self::generate_from(forbidden_parts, |buf| {
            getrandom::fill(buf).map_err(|e| PasswordError::Random(e.to_string()))
        })
    }

    /// A new password from `fill`, which writes random bytes. Tests pass a
    /// fixed source.
    pub fn generate_from(
        forbidden_parts: &[String],
        mut fill: impl FnMut(&mut [u8]) -> Result<(), PasswordError>,
    ) -> Result<Self, PasswordError> {
        let alphabet = alphabet();
        // Largest multiple of the alphabet size below 256: bytes at or above
        // it are thrown away, so each character is equally likely.
        let limit = (256 / alphabet.len()) * alphabet.len();
        let mut bytes = Zeroizing::new([0u8; 64]);
        for _ in 0..MAX_ATTEMPTS {
            let mut out = Zeroizing::new(String::with_capacity(PASSWORD_LEN));
            // Bounded: a source that never yields a usable byte ends with
            // `Exhausted` instead of spinning.
            for _ in 0..16 {
                if out.len() == PASSWORD_LEN {
                    break;
                }
                fill(&mut bytes[..])?;
                for b in bytes.iter() {
                    if out.len() == PASSWORD_LEN {
                        break;
                    }
                    if usize::from(*b) < limit {
                        out.push(char::from(alphabet[usize::from(*b) % alphabet.len()]));
                    }
                }
            }
            if out.len() == PASSWORD_LEN && meets_rules(&out, forbidden_parts) {
                return Ok(Password(out));
            }
        }
        Err(PasswordError::Exhausted)
    }

    /// A password received from the elevated setup step. Checked against the
    /// alphabet and the length so a damaged message is refused, not stored.
    pub fn from_received(text: String) -> Result<Self, PasswordError> {
        let text = Zeroizing::new(text);
        if text.len() != PASSWORD_LEN || !text.bytes().all(|b| alphabet().contains(&b)) {
            return Err(PasswordError::Malformed);
        }
        Ok(Password(text))
    }

    /// The password text, for the one Windows call that sets it and the one
    /// that uses it. Never printed or logged.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Password {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Password(<redacted>)")
    }
}

fn alphabet() -> Vec<u8> {
    let mut a: Vec<u8> = (b'A'..=b'Z')
        .chain(b'a'..=b'z')
        .chain(b'0'..=b'9')
        .collect();
    a.extend_from_slice(PASSWORD_SYMBOLS.as_bytes());
    a
}

fn meets_rules(candidate: &str, forbidden_parts: &[String]) -> bool {
    let has = |f: fn(&u8) -> bool| candidate.as_bytes().iter().any(f);
    let lower = candidate.to_ascii_lowercase();
    has(u8::is_ascii_uppercase)
        && has(u8::is_ascii_lowercase)
        && has(u8::is_ascii_digit)
        && has(|b| PASSWORD_SYMBOLS.as_bytes().contains(b))
        && !forbidden_parts
            .iter()
            .any(|p| !p.is_empty() && lower.contains(&p.to_ascii_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed pseudo-random source (xorshift64), so a test sees the same
    /// candidates each run.
    fn counter_source() -> impl FnMut(&mut [u8]) -> Result<(), PasswordError> {
        let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
        move |buf: &mut [u8]| {
            for b in buf.iter_mut() {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                *b = (x >> 24) as u8;
            }
            Ok(())
        }
    }

    #[test]
    fn generated_passwords_meet_length_charset_and_classes() {
        let forbidden = vec!["kgraf".to_owned(), "off".to_owned()];
        for _ in 0..200 {
            let p = Password::generate(&forbidden).unwrap();
            let s = p.expose();
            assert_eq!(s.len(), PASSWORD_LEN);
            assert!(s.bytes().all(|b| alphabet().contains(&b)), "{}", s.len());
            assert!(meets_rules(s, &forbidden));
            assert!(!s.contains(char::is_whitespace));
            assert!(!s.contains(['"', '\'', '\\', '\n', '\r', '$', '`', '&', '|', ';']));
        }
    }

    #[test]
    fn two_passwords_differ() {
        let a = Password::generate(&[]).unwrap();
        let b = Password::generate(&[]).unwrap();
        assert_ne!(a.expose(), b.expose());
    }

    #[test]
    fn a_constant_source_never_yields_a_password() {
        // All zero bytes give "AAAA...": no lowercase, digit or symbol.
        let r = Password::generate_from(&[], |buf| {
            buf.fill(0);
            Ok(())
        });
        assert!(matches!(r, Err(PasswordError::Exhausted)));
        // All bytes above the rejection limit: nothing is ever accepted.
        let r = Password::generate_from(&[], |buf| {
            buf.fill(255);
            Ok(())
        });
        assert!(matches!(r, Err(PasswordError::Exhausted)));
    }

    #[test]
    fn rejection_sampling_drops_bytes_at_or_above_the_limit() {
        let n = alphabet().len();
        let limit = (256 / n) * n;
        // Bytes alternate: one above the limit (dropped), then a counter.
        let mut i: u8 = 0;
        let p = Password::generate_from(&[], |buf| {
            for (k, b) in buf.iter_mut().enumerate() {
                *b = if k % 2 == 0 {
                    255
                } else {
                    i = i.wrapping_add(7) % (limit as u8);
                    i
                };
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(p.expose().len(), PASSWORD_LEN);
    }

    #[test]
    fn a_name_part_in_the_password_is_refused() {
        assert!(!meets_rules("Aa1!xxOFFxx", &["off".to_owned()]));
        assert!(meets_rules("Aa1!xxOFxx", &["off".to_owned()]));
        // The deterministic source gives the same candidate list each time;
        // forbidding a part of the first candidate moves to a later one.
        let first = Password::generate_from(&[], counter_source()).unwrap();
        let part = first.expose()[..3].to_owned();
        let second =
            Password::generate_from(std::slice::from_ref(&part), counter_source()).unwrap();
        assert!(
            !second
                .expose()
                .to_ascii_lowercase()
                .contains(&part.to_ascii_lowercase())
        );
    }

    #[test]
    fn debug_never_shows_the_text() {
        let p = Password::generate(&[]).unwrap();
        let shown = format!("{p:?}");
        assert!(!shown.contains(p.expose()));
        assert!(shown.contains("redacted"));
    }

    #[test]
    fn received_passwords_are_checked() {
        let good = Password::generate(&[]).unwrap().expose().to_owned();
        assert!(Password::from_received(good.clone()).is_ok());
        assert!(Password::from_received(good[..10].to_owned()).is_err());
        let mut spaced = good.clone();
        spaced.replace_range(0..1, " ");
        assert!(Password::from_received(spaced).is_err());
    }
}
