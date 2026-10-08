//! The messages the elevated step sends the owner's process over the setup
//! pipe.
//!
//! One direction (elevated to owner), one message per line, at most
//! [`MAX_LINE`] bytes:
//!
//! ```text
//! kgraf-sandbox-pipe 1 <nonce>        first: the nonce from the command line
//! step <key> <outcome>                each step that ran
//! fail <key> <message>                the step that failed; nothing follows
//! password <role> <password>          setup only, one per user
//! end                                 last, after success
//! ```
//!
//! The nonce, given to the elevated step on its command line, ties the pipe
//! to this one setup run, beside the process checks on both ends (decision
//! 0097). A failed step's message is cut to one line. A password is checked
//! against its alphabet on arrival ([`Password::from_received`]).

use std::fmt;

use super::names::Role;
use super::password::Password;

pub const PIPE_PROTOCOL: &str = "kgraf-sandbox-pipe";
pub const PIPE_VERSION: u32 = 1;

/// The longest line either side accepts.
pub const MAX_LINE: usize = 4096;

/// Hex digits in a nonce (128 bits).
pub const NONCE_HEX: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepOutcome {
    Created,
    Updated,
    Verified,
    Removed,
    AlreadyGone,
}

impl StepOutcome {
    pub fn word(self) -> &'static str {
        match self {
            StepOutcome::Created => "created",
            StepOutcome::Updated => "updated",
            StepOutcome::Verified => "verified",
            StepOutcome::Removed => "removed",
            StepOutcome::AlreadyGone => "already-gone",
        }
    }

    fn from_word(w: &str) -> Option<Self> {
        [
            StepOutcome::Created,
            StepOutcome::Updated,
            StepOutcome::Verified,
            StepOutcome::Removed,
            StepOutcome::AlreadyGone,
        ]
        .into_iter()
        .find(|o| o.word() == w)
    }
}

impl fmt::Display for StepOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.word())
    }
}

#[derive(Debug)]
pub enum Message {
    Hello { nonce: String },
    Step { key: String, outcome: StepOutcome },
    Fail { key: String, message: String },
    Password { role: Role, password: Password },
    End,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum HandshakeError {
    #[error("a setup pipe message is longer than {MAX_LINE} bytes")]
    TooLong,
    #[error("a setup pipe message is not understood")]
    Malformed,
    #[error("the setup pipe speaks another protocol or version")]
    Version,
    #[error("the setup pipe's nonce does not match this run's")]
    Nonce,
    #[error("the setup pipe's messages are out of order")]
    Order,
    #[error("the setup pipe closed before the elevated step finished")]
    Closed,
}

/// A fresh nonce from the system random source.
pub fn new_nonce() -> Result<String, getrandom::Error> {
    let mut b = [0u8; NONCE_HEX / 2];
    getrandom::fill(&mut b)?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

pub fn is_nonce(s: &str) -> bool {
    s.len() == NONCE_HEX && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The pipe's name for one run.
pub fn pipe_name(nonce: &str) -> String {
    format!(r"\\.\pipe\kgraf-sandbox-setup-{nonce}")
}

/// Cuts a message to one line of printable text.
fn one_line(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let cap = MAX_LINE - 512;
    if out.len() > cap {
        let mut end = cap;
        while !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
    }
    out
}

impl Message {
    /// One line, with its newline.
    pub fn encode(&self) -> String {
        match self {
            Message::Hello { nonce } => format!("{PIPE_PROTOCOL} {PIPE_VERSION} {nonce}\n"),
            Message::Step { key, outcome } => format!("step {key} {outcome}\n"),
            Message::Fail { key, message } => format!("fail {key} {}\n", one_line(message)),
            Message::Password { role, password } => {
                format!("password {role} {}\n", password.expose())
            }
            Message::End => "end\n".to_owned(),
        }
    }

    pub fn parse(line: &str) -> Result<Message, HandshakeError> {
        if line.len() > MAX_LINE {
            return Err(HandshakeError::TooLong);
        }
        let line = line.strip_suffix('\n').unwrap_or(line);
        let line = line.strip_suffix('\r').unwrap_or(line);
        let mut words = line.splitn(3, ' ');
        let first = words.next().unwrap_or_default();
        let second = words.next();
        let rest = words.next();
        let key_ok = |k: &str| {
            !k.is_empty()
                && k.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b':' | b'.'))
        };
        match (first, second, rest) {
            (PIPE_PROTOCOL, Some(v), Some(nonce)) => {
                if v != PIPE_VERSION.to_string() {
                    return Err(HandshakeError::Version);
                }
                if !is_nonce(nonce) {
                    return Err(HandshakeError::Malformed);
                }
                Ok(Message::Hello {
                    nonce: nonce.to_owned(),
                })
            }
            (PIPE_PROTOCOL, _, _) => Err(HandshakeError::Version),
            ("step", Some(key), Some(word)) if key_ok(key) => Ok(Message::Step {
                key: key.to_owned(),
                outcome: StepOutcome::from_word(word).ok_or(HandshakeError::Malformed)?,
            }),
            ("fail", Some(key), message) if key_ok(key) => Ok(Message::Fail {
                key: key.to_owned(),
                message: message.unwrap_or_default().to_owned(),
            }),
            ("password", Some(role), Some(pw)) => Ok(Message::Password {
                role: Role::from_key(role).ok_or(HandshakeError::Malformed)?,
                password: Password::from_received(pw.to_owned())
                    .map_err(|_| HandshakeError::Malformed)?,
            }),
            ("end", None, None) => Ok(Message::End),
            _ => Err(HandshakeError::Malformed),
        }
    }
}

/// The owner's side: checks the order of the messages as they arrive.
#[derive(Debug, Default)]
pub struct Receiver {
    state: ReceiverState,
    passwords: Vec<(Role, Password)>,
}

#[derive(Debug, Default, PartialEq, Eq)]
enum ReceiverState {
    #[default]
    AwaitHello,
    Steps,
    Passwords,
    Failed,
    Done,
}

/// What a received line means for the owner's terminal.
#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    Started,
    Step { key: String, outcome: StepOutcome },
    Failed { key: String, message: String },
    PasswordReceived(Role),
    Finished,
}

impl Receiver {
    pub fn new() -> Self {
        Self::default()
    }

    /// Takes one line. `nonce` is the one this run passed on the command line.
    pub fn take(&mut self, line: &str, nonce: &str) -> Result<Event, HandshakeError> {
        use ReceiverState as S;
        let msg = Message::parse(line)?;
        match (&self.state, msg) {
            (S::AwaitHello, Message::Hello { nonce: got }) => {
                if got != nonce {
                    return Err(HandshakeError::Nonce);
                }
                self.state = S::Steps;
                Ok(Event::Started)
            }
            (S::Steps, Message::Step { key, outcome }) => Ok(Event::Step { key, outcome }),
            (S::Steps | S::Passwords, Message::Fail { key, message }) => {
                self.state = S::Failed;
                Ok(Event::Failed { key, message })
            }
            (S::Steps | S::Passwords, Message::Password { role, password }) => {
                if self.passwords.iter().any(|(r, _)| *r == role) {
                    return Err(HandshakeError::Order);
                }
                self.state = S::Passwords;
                self.passwords.push((role, password));
                Ok(Event::PasswordReceived(role))
            }
            (S::Steps | S::Passwords, Message::End) => {
                self.state = S::Done;
                Ok(Event::Finished)
            }
            _ => Err(HandshakeError::Order),
        }
    }

    pub fn finished(&self) -> bool {
        self.state == ReceiverState::Done
    }

    pub fn failed(&self) -> bool {
        self.state == ReceiverState::Failed
    }

    /// The passwords, once the run finished.
    pub fn into_passwords(self) -> Result<Vec<(Role, Password)>, HandshakeError> {
        if self.state != ReceiverState::Done {
            return Err(HandshakeError::Closed);
        }
        Ok(self.passwords)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nonce() -> String {
        "0123456789abcdef0123456789abcdef".into()
    }

    #[test]
    fn every_message_round_trips_through_one_line() {
        let pw = Password::generate(&[]).unwrap();
        let text = pw.expose().to_owned();
        let msgs = vec![
            Message::Hello { nonce: nonce() },
            Message::Step {
                key: "user-offline".into(),
                outcome: StepOutcome::Created,
            },
            Message::Step {
                key: "remove-rule:kgraf-sandbox-1a2b3c4d-block-inbound".into(),
                outcome: StepOutcome::AlreadyGone,
            },
            Message::Fail {
                key: "rule-block-outbound".into(),
                message: "Rules::Add failed: 0x80070005\r\nsecond line".into(),
            },
            Message::Password {
                role: Role::Online,
                password: pw,
            },
            Message::End,
        ];
        for m in msgs {
            let line = m.encode();
            assert!(line.ends_with('\n'));
            assert_eq!(line.matches('\n').count(), 1, "one line: {line:?}");
            let back = Message::parse(&line).unwrap();
            assert_eq!(back.encode(), line);
        }
        let fail = Message::parse("fail x a\tb\n").unwrap();
        assert!(matches!(fail, Message::Fail { .. }));
        let m = Message::Password {
            role: Role::Offline,
            password: Password::from_received(text.clone()).unwrap(),
        };
        assert!(
            !format!("{m:?}").contains(&text),
            "Debug never shows a password"
        );
    }

    #[test]
    fn bad_lines_are_refused() {
        for bad in [
            "",
            "hello",
            "step",
            "step user-offline",
            "step user-offline exploded",
            "step bad key created",
            "password admin Aa1!",
            "password offline short",
            "end now",
            "kgraf-sandbox-pipe 1 not-a-nonce",
        ] {
            assert!(Message::parse(bad).is_err(), "{bad:?}");
        }
        assert_eq!(
            Message::parse(&format!("kgraf-sandbox-pipe 2 {}", nonce())).unwrap_err(),
            HandshakeError::Version
        );
        let long = format!("fail k {}", "x".repeat(MAX_LINE));
        assert_eq!(Message::parse(&long).unwrap_err(), HandshakeError::TooLong);
        // A long failure message is cut so its line still parses.
        let line = Message::Fail {
            key: "k".into(),
            message: "y".repeat(MAX_LINE * 2),
        }
        .encode();
        assert!(line.len() <= MAX_LINE);
        assert!(Message::parse(&line).is_ok());
    }

    #[test]
    fn the_receiver_accepts_a_run_in_order() {
        let pw = Password::generate(&[]).unwrap().expose().to_owned();
        let mut r = Receiver::new();
        let n = nonce();
        assert_eq!(
            r.take(&Message::Hello { nonce: n.clone() }.encode(), &n)
                .unwrap(),
            Event::Started
        );
        assert!(matches!(
            r.take("step user-offline created\n", &n).unwrap(),
            Event::Step { .. }
        ));
        assert_eq!(
            r.take(&format!("password offline {pw}\n"), &n).unwrap(),
            Event::PasswordReceived(Role::Offline)
        );
        assert_eq!(
            r.take(&format!("password online {pw}\n"), &n).unwrap(),
            Event::PasswordReceived(Role::Online)
        );
        assert_eq!(r.take("end\n", &n).unwrap(), Event::Finished);
        assert!(r.finished());
        let pws = r.into_passwords().unwrap();
        assert_eq!(pws.len(), 2);
        assert_eq!(pws[0].1.expose(), pw);
    }

    #[test]
    fn the_receiver_refuses_a_wrong_nonce_and_a_wrong_order() {
        let n = nonce();
        let mut r = Receiver::new();
        let other = "ffffffffffffffffffffffffffffffff";
        assert_eq!(
            r.take(&format!("kgraf-sandbox-pipe 1 {other}\n"), &n)
                .unwrap_err(),
            HandshakeError::Nonce
        );
        let mut r = Receiver::new();
        assert_eq!(
            r.take("step user-offline created\n", &n).unwrap_err(),
            HandshakeError::Order
        );
        let mut r = Receiver::new();
        r.take(&format!("kgraf-sandbox-pipe 1 {n}\n"), &n).unwrap();
        assert_eq!(
            r.take(&format!("kgraf-sandbox-pipe 1 {n}\n"), &n)
                .unwrap_err(),
            HandshakeError::Order
        );
        // Steps after passwords are out of order; so is a second password for
        // one role.
        let pw = Password::generate(&[]).unwrap().expose().to_owned();
        r.take(&format!("password offline {pw}\n"), &n).unwrap();
        assert_eq!(
            r.take("step group created\n", &n).unwrap_err(),
            HandshakeError::Order
        );
        assert_eq!(
            r.take(&format!("password offline {pw}\n"), &n).unwrap_err(),
            HandshakeError::Order
        );
    }

    #[test]
    fn a_failed_or_cut_run_yields_no_passwords() {
        let n = nonce();
        let mut r = Receiver::new();
        r.take(&format!("kgraf-sandbox-pipe 1 {n}\n"), &n).unwrap();
        assert!(matches!(
            r.take("fail rule-block-outbound access denied\n", &n)
                .unwrap(),
            Event::Failed { .. }
        ));
        assert!(r.failed());
        assert_eq!(r.take("end\n", &n).unwrap_err(), HandshakeError::Order);
        assert_eq!(r.into_passwords().unwrap_err(), HandshakeError::Closed);
        let mut r = Receiver::new();
        r.take(&format!("kgraf-sandbox-pipe 1 {n}\n"), &n).unwrap();
        assert_eq!(r.into_passwords().unwrap_err(), HandshakeError::Closed);
    }

    #[test]
    fn nonces_and_pipe_names() {
        let a = new_nonce().unwrap();
        assert!(is_nonce(&a));
        assert_ne!(a, new_nonce().unwrap());
        assert!(pipe_name(&a).starts_with(r"\\.\pipe\kgraf-sandbox-setup-"));
        assert!(!is_nonce("0123"));
        assert!(!is_nonce("0123456789ABCDEF0123456789ABCDEF"));
    }
}
