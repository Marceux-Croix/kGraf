//! The messages the elevated step sends the owner's process over the setup
//! pipe.
//!
//! One direction (elevated to owner), one message per line, at most
//! [`MAX_LINE`] bytes:
//!
//! ```text
//! kgraf-sandbox-pipe 1 <nonce>        first: the nonce from the command line
//! step <key> <outcome>                each step that ran
//! note <key> <message>                left alone, with why; the run goes on
//! gap <key> <message>                 a part Windows refused; the run goes on
//! fail <key> <message>                the step that failed; nothing follows
//! password <role> <password>          setup only, one per user
//! end                                 last, after success
//! ```
//!
//! The nonce, given to the elevated step on its command line, ties the pipe
//! to this one setup run, beside the process checks on both ends (decision
//! 0097). A failed step's message is cut to one line. A password is checked
//! against its alphabet on arrival ([`Password::from_received`]).
//!
//! A key is one word on the line. A removal key carries a folder path
//! (`remove-closed:C:\Users\Public`), which can hold a backslash, a space or
//! any other character, so a key is written with every byte outside
//! [`is_plain_key_byte`] as `%XX` ([`escape_key`]) and read back to the same
//! text ([`unescape_key`]). A raw path in a key would make the owner's side
//! refuse the line, and a removal that stopped there would leave everything
//! after it on the machine.

use std::fmt;
use std::io::Read;

use zeroize::Zeroizing;

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
    /// The step ran and part of it was refused; the `gap` lines before it
    /// say which part and why.
    Partial,
}

impl StepOutcome {
    pub fn word(self) -> &'static str {
        match self {
            StepOutcome::Created => "created",
            StepOutcome::Updated => "updated",
            StepOutcome::Verified => "verified",
            StepOutcome::Removed => "removed",
            StepOutcome::AlreadyGone => "already-gone",
            StepOutcome::Partial => "partial",
        }
    }

    fn from_word(w: &str) -> Option<Self> {
        [
            StepOutcome::Created,
            StepOutcome::Updated,
            StepOutcome::Verified,
            StepOutcome::Removed,
            StepOutcome::AlreadyGone,
            StepOutcome::Partial,
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
    Hello {
        nonce: String,
    },
    Step {
        key: String,
        outcome: StepOutcome,
    },
    /// Something the step left alone, with why. Unlike [`Message::Fail`] the
    /// run goes on: a removal that cannot undo one entry still undoes the
    /// rest, and the entry stays in the undo record.
    Note {
        key: String,
        message: String,
    },
    /// A part of a step that Windows refused (a DNS filter), with why. The
    /// run goes on without it, and the owner's process reports it as a gap
    /// at the end ([`Receiver::gaps`]).
    Gap {
        key: String,
        message: String,
    },
    Fail {
        key: String,
        message: String,
    },
    Password {
        role: Role,
        password: Password,
    },
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

/// A byte a key may carry as it is. Every other byte, `%` among them, is
/// written as `%XX`.
pub fn is_plain_key_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b':' | b'.')
}

/// The longest key on the wire, escapes counted. A longer key is cut at a
/// character and ends in `...`, so the line stays under [`MAX_LINE`] with its
/// message: a key is for the owner's terminal, and the undo record, not the
/// key, says what is left.
pub const MAX_KEY: usize = 1024;

/// `key` as one word of [`is_plain_key_byte`] bytes and `%XX` escapes, at
/// most [`MAX_KEY`] bytes long.
pub fn escape_key(key: &str) -> String {
    const CUT: &str = "...";
    let piece = |c: char| -> String {
        let mut buf = [0u8; 4];
        c.encode_utf8(&mut buf)
            .bytes()
            .map(|b| {
                if is_plain_key_byte(b) {
                    char::from(b).to_string()
                } else {
                    format!("%{b:02X}")
                }
            })
            .collect()
    };
    let whole: String = key.chars().map(piece).collect();
    if whole.len() <= MAX_KEY {
        return whole;
    }
    // Cut at a character, so no escape and no UTF-8 sequence is split.
    let mut out = String::with_capacity(MAX_KEY);
    for c in key.chars() {
        let p = piece(c);
        if out.len() + p.len() > MAX_KEY - CUT.len() {
            break;
        }
        out.push_str(&p);
    }
    out.push_str(CUT);
    out
}

/// The key [`escape_key`] wrote, or `None` for a word it cannot have written:
/// empty, a byte outside the plain set, a `%` not followed by two hex digits,
/// or bytes that are not UTF-8.
pub fn unescape_key(word: &str) -> Option<String> {
    if word.is_empty() {
        return None;
    }
    let bytes = word.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hex = word.get(i + 1..i + 3)?;
                if !hex.bytes().all(|h| h.is_ascii_hexdigit()) {
                    return None;
                }
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 3;
            }
            b if is_plain_key_byte(b) => {
                out.push(b);
                i += 1;
            }
            _ => return None,
        }
    }
    String::from_utf8(out).ok()
}

/// Cuts a message to one line of printable text.
fn one_line(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    // Room for the kind, the longest key and the newline.
    let cap = MAX_LINE - MAX_KEY - 64;
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
    /// One line, with its newline. Cleared when dropped, because a password
    /// line carries the password.
    pub fn encode(&self) -> Zeroizing<String> {
        Zeroizing::new(match self {
            Message::Hello { nonce } => format!("{PIPE_PROTOCOL} {PIPE_VERSION} {nonce}\n"),
            Message::Step { key, outcome } => format!("step {} {outcome}\n", escape_key(key)),
            Message::Note { key, message } => {
                format!("note {} {}\n", escape_key(key), one_line(message))
            }
            Message::Gap { key, message } => {
                format!("gap {} {}\n", escape_key(key), one_line(message))
            }
            Message::Fail { key, message } => {
                format!("fail {} {}\n", escape_key(key), one_line(message))
            }
            Message::Password { role, password } => {
                return password_line(*role, password);
            }
            Message::End => "end\n".to_owned(),
        })
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
            ("step", Some(key), Some(word)) => Ok(Message::Step {
                key: unescape_key(key).ok_or(HandshakeError::Malformed)?,
                outcome: StepOutcome::from_word(word).ok_or(HandshakeError::Malformed)?,
            }),
            ("note", Some(key), message) => Ok(Message::Note {
                key: unescape_key(key).ok_or(HandshakeError::Malformed)?,
                message: message.unwrap_or_default().to_owned(),
            }),
            ("gap", Some(key), message) => Ok(Message::Gap {
                key: unescape_key(key).ok_or(HandshakeError::Malformed)?,
                message: message.unwrap_or_default().to_owned(),
            }),
            ("fail", Some(key), message) => Ok(Message::Fail {
                key: unescape_key(key).ok_or(HandshakeError::Malformed)?,
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

/// `password <role> <password>\n` in one buffer allocated at its final size,
/// so no reallocation leaves a copy of the password behind that nothing
/// clears (`format!` grows its buffer as it writes).
fn password_line(role: Role, password: &Password) -> Zeroizing<String> {
    let role = role.to_string();
    let text = password.expose();
    let len = "password ".len() + role.len() + 1 + text.len() + 1;
    let mut out = Zeroizing::new(String::with_capacity(len));
    out.push_str("password ");
    out.push_str(&role);
    out.push(' ');
    out.push_str(text);
    out.push('\n');
    out
}

/// The lines of a setup pipe, each in a buffer cleared when it is dropped.
///
/// `BufRead::lines` would leave the password lines in its read buffer and in
/// every buffer a growing `String` gives up, and none of those is cleared.
/// Here the read buffer and the line are allocated once, at their final
/// size, and both are cleared. A line longer than [`MAX_LINE`] ends the
/// stream with an error, as it can come from no correct elevated step.
pub struct SecretLines<R> {
    reader: R,
    chunk: Zeroizing<Vec<u8>>,
    /// Bytes read but not yet returned: `chunk[start..end]`.
    start: usize,
    end: usize,
    done: bool,
}

impl<R: Read> SecretLines<R> {
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            chunk: Zeroizing::new(vec![0u8; MAX_LINE]),
            start: 0,
            end: 0,
            done: false,
        }
    }

    fn next_line(&mut self) -> std::io::Result<Option<Zeroizing<String>>> {
        // One allocation of the largest size a line may have: pushing never
        // reallocates, so the bytes exist only here and in `chunk`.
        let mut line = Zeroizing::new(Vec::with_capacity(MAX_LINE));
        loop {
            if self.start == self.end {
                let n = self.reader.read(&mut self.chunk[..])?;
                if n == 0 {
                    if line.is_empty() {
                        return Ok(None);
                    }
                    break;
                }
                self.start = 0;
                self.end = n;
            }
            let available = &self.chunk[self.start..self.end];
            let (take, newline) = match available.iter().position(|b| *b == b'\n') {
                Some(i) => (i + 1, true),
                None => (available.len(), false),
            };
            if line.len() + take > MAX_LINE {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    HandshakeError::TooLong,
                ));
            }
            line.extend_from_slice(&available[..take]);
            self.start += take;
            if newline {
                line.pop();
                break;
            }
        }
        // Moves the bytes into the String without a copy.
        let bytes = std::mem::take(&mut *line);
        match String::from_utf8(bytes) {
            Ok(text) => Ok(Some(Zeroizing::new(text))),
            Err(e) => {
                // Clear the bytes the failed conversion hands back.
                drop(Zeroizing::new(e.into_bytes()));
                Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    HandshakeError::Malformed,
                ))
            }
        }
    }
}

impl<R: Read> Iterator for SecretLines<R> {
    type Item = std::io::Result<Zeroizing<String>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let out = self.next_line().transpose();
        if !matches!(out, Some(Ok(_))) {
            self.done = true;
        }
        out
    }
}

/// The owner's side: checks the order of the messages as they arrive.
#[derive(Debug, Default)]
pub struct Receiver {
    state: ReceiverState,
    passwords: Vec<(Role, Password)>,
    /// Each part a step reported as refused ([`Message::Gap`]), in order.
    gaps: Vec<(String, String)>,
    /// The name of each line after the greeting that did not parse
    /// ([`unparsed_name`]), in order.
    unparsed: Vec<String>,
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
    Step {
        key: String,
        outcome: StepOutcome,
    },
    /// Left alone, with why; the run goes on.
    Note {
        key: String,
        message: String,
    },
    /// Refused by Windows, with why; the run goes on without it.
    Gap {
        key: String,
        message: String,
    },
    Failed {
        key: String,
        message: String,
    },
    PasswordReceived(Role),
    /// A line after the greeting that did not parse, by name
    /// ([`unparsed_name`]). The run goes on: the elevated step is still
    /// removing or setting up, and it stops at its next message if this side
    /// stops reading. The owner's process fails at the end
    /// ([`Receiver::unparsed`]).
    Unparsed(String),
    Finished,
}

/// What a line that did not parse was, without what it carried: its kind
/// when that is one this protocol knows, and for a step, note or failure the
/// key as it arrived, cut to [`MAX_KEY`] bytes of printable text. A password
/// line is named by its kind alone, so no part of a password reaches a
/// terminal or a log.
pub fn unparsed_name(line: &str) -> String {
    let line = line.trim_end_matches(['\n', '\r']);
    let mut words = line.splitn(3, ' ');
    let kind = words.next().unwrap_or_default();
    match kind {
        "step" | "note" | "gap" | "fail" => {
            let key = one_line(words.next().unwrap_or_default());
            let mut end = key.len().min(MAX_KEY);
            while !key.is_char_boundary(end) {
                end -= 1;
            }
            format!("{kind} {}", &key[..end])
        }
        "password" | "end" | PIPE_PROTOCOL => kind.to_owned(),
        _ => "a line of no known kind".to_owned(),
    }
}

impl Receiver {
    pub fn new() -> Self {
        Self::default()
    }

    /// Takes one line. `nonce` is the one this run passed on the command line.
    pub fn take(&mut self, line: &str, nonce: &str) -> Result<Event, HandshakeError> {
        use ReceiverState as S;
        let msg = match Message::parse(line) {
            Ok(msg) => msg,
            // After the greeting the pipe is known to be this run's, so a
            // line this side cannot read is named and passed over, and the
            // rest of the run is still read: a removal that stopped here
            // would leave everything after the line on the machine.
            // Before it, nothing on the pipe is trusted yet.
            Err(HandshakeError::Malformed) if matches!(self.state, S::Steps | S::Passwords) => {
                let name = unparsed_name(line);
                self.unparsed.push(name.clone());
                return Ok(Event::Unparsed(name));
            }
            Err(e) => return Err(e),
        };
        match (&self.state, msg) {
            (S::AwaitHello, Message::Hello { nonce: got }) => {
                if got != nonce {
                    return Err(HandshakeError::Nonce);
                }
                self.state = S::Steps;
                Ok(Event::Started)
            }
            (S::Steps, Message::Step { key, outcome }) => Ok(Event::Step { key, outcome }),
            (S::Steps, Message::Note { key, message }) => Ok(Event::Note { key, message }),
            (S::Steps, Message::Gap { key, message }) => {
                self.gaps.push((key.clone(), message.clone()));
                Ok(Event::Gap { key, message })
            }
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

    /// The names of the lines that did not parse ([`Event::Unparsed`]). A
    /// run with any is a failed run, whatever else it reported.
    pub fn unparsed(&self) -> &[String] {
        &self.unparsed
    }

    /// Each part a step reported as refused, by key, with why.
    pub fn gaps(&self) -> &[(String, String)] {
        &self.gaps
    }

    /// The passwords, once the run finished.
    pub fn into_passwords(self) -> Result<Vec<(Role, Password)>, HandshakeError> {
        if self.state != ReceiverState::Done {
            return Err(HandshakeError::Closed);
        }
        if !self.unparsed.is_empty() {
            return Err(HandshakeError::Malformed);
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
    fn a_password_line_is_built_at_its_final_size() {
        // A buffer that grew while the password was written into it gave up
        // smaller buffers holding a prefix of the password, which nothing
        // clears. One allocation of the exact size leaves none.
        let pw = Password::generate(&[]).unwrap();
        let line = Message::Password {
            role: Role::Slot(3),
            password: pw,
        }
        .encode();
        assert_eq!(line.capacity(), line.len(), "{}", line.len());
        assert!(Message::parse(&line).is_ok());
    }

    /// A reader that hands out at most `step` bytes per call, so a line
    /// arrives in pieces as it can from a pipe.
    struct Trickle<'a> {
        data: &'a [u8],
        step: usize,
    }

    impl std::io::Read for Trickle<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.step.min(buf.len()).min(self.data.len());
            buf[..n].copy_from_slice(&self.data[..n]);
            self.data = &self.data[n..];
            Ok(n)
        }
    }

    #[test]
    fn secret_lines_split_like_lines_and_refuse_a_long_line() {
        let pw = Password::generate(&[]).unwrap().expose().to_owned();
        let text = format!(
            "kgraf-sandbox-pipe 1 {}\npassword offline {pw}\r\nend",
            nonce()
        );
        for step in [1, 3, 7, 4096] {
            let got: Vec<String> = SecretLines::new(Trickle {
                data: text.as_bytes(),
                step,
            })
            .map(|l| l.unwrap().to_string())
            .collect();
            let want: Vec<String> = std::io::BufRead::lines(text.as_bytes())
                .map(|l| l.unwrap())
                .collect();
            // `lines` also drops a `\r`; `Message::parse` does that here.
            assert_eq!(got.len(), want.len(), "step {step}");
            for (g, w) in got.iter().zip(&want) {
                assert_eq!(g.trim_end_matches('\r'), w, "step {step}");
            }
        }
        let long = format!("fail k {}\nend\n", "x".repeat(MAX_LINE));
        let mut lines = SecretLines::new(long.as_bytes());
        let e = lines.next().unwrap().unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::InvalidData);
        assert!(lines.next().is_none(), "nothing after an error");
        let mut bad = SecretLines::new(&b"step \xff created\n"[..]);
        assert!(bad.next().unwrap().is_err());
        assert!(SecretLines::new(&b""[..]).next().is_none());
    }

    /// Keys as a removal writes them, with what a folder path can hold.
    const AWKWARD_KEYS: [&str; 8] = [
        r"remove-closed:C:\",
        r"remove-closed:C:\Users\Public",
        r"remove-closed:C:\Users\Public\Account Pictures",
        r"remove-closed:D:\données partagées",
        r"remove-closed:C:\100% done\%41",
        "remove-closed:C:\\tab\there\r\nend",
        "remove-closed:C:\\\u{1F600}",
        "plain-key_1.2:x",
    ];

    #[test]
    fn a_key_round_trips_whatever_it_holds() {
        for key in AWKWARD_KEYS {
            let word = escape_key(key);
            assert!(
                word.bytes().all(|b| is_plain_key_byte(b) || b == b'%'),
                "{word}"
            );
            assert_eq!(unescape_key(&word).as_deref(), Some(key), "{word}");
        }
        // A plain key is written as it is, so a key an older elevated step
        // sent still reads the same.
        assert_eq!(
            escape_key("remove-rule:kgraf-sandbox-1a2b"),
            "remove-rule:kgraf-sandbox-1a2b"
        );
        for bad in ["", "a b", r"C:\", "%", "%4", "%zz", "%FF", "a%2"] {
            assert_eq!(unescape_key(bad), None, "{bad:?}");
        }
        // A key too long for a line is cut at a character, and still reads.
        let long = format!("remove-closed:C:\\{}", "\u{e9}".repeat(MAX_KEY));
        let word = escape_key(&long);
        assert!(
            word.len() <= MAX_KEY && word.ends_with("..."),
            "{}",
            word.len()
        );
        let back = unescape_key(&word).expect("a cut key still reads");
        assert!(long.starts_with(back.trim_end_matches('.')));
        let line = Message::Fail {
            key: long,
            message: "y".repeat(MAX_LINE * 2),
        }
        .encode();
        assert!(line.len() <= MAX_LINE, "{}", line.len());
        assert!(Message::parse(&line).is_ok());
    }

    /// Every message the elevated step sends, for a setup and for a
    /// removal, read by a single receiver on the owner's side: each key comes
    /// out as it went in. A removal sends a key per undo record entry, and a folder's
    /// key carries its path, which the owner's side once refused.
    #[test]
    fn every_message_the_elevated_step_sends_reaches_the_owner_intact() {
        use super::super::acl::{CLOSED_FOLDER_DENY, CLOSED_FOLDER_FLAGS};
        use super::super::names::{MAX_SLOTS, OwnerNames};
        use super::super::plan::{
            Observed, StepId, failure_leaves, plan_remove, plan_setup, undo_key,
        };
        use super::super::record::{DENIED_LOGON_RIGHTS, Record, UndoEntry};
        use super::super::wfp::WfpKeys;

        let n = OwnerNames::derive("S-1-5-21-1-2-3-1001", Some("t1"))
            .unwrap()
            .with_slots(MAX_SLOTS);
        let mut observed = Observed::default();
        observed.firewall.enabled_all_profiles = Some(true);
        observed.firewall.local_policy_effective = Some(true);
        let setup = plan_setup(&n, &observed).unwrap();
        let mut keys: Vec<String> = setup.iter().map(|s| s.id.key()).collect();
        assert!(keys.contains(&StepId::CloseFolders.key()));

        // A record with an entry of every kind, the deny entries on folders
        // whose paths hold what a path can.
        let mut record = Record::new(n.owner_sid(), n.tag());
        for role in n.roles() {
            let user = n.user(role);
            record.add(UndoEntry::User(user.clone()));
            record.add(UndoEntry::Hidden(user.clone()));
            record.add(UndoEntry::Rights {
                user,
                rights: DENIED_LOGON_RIGHTS
                    .iter()
                    .map(|r| (*r).to_owned())
                    .collect(),
            });
        }
        record.add(UndoEntry::Group(n.group()));
        for rule in n.rules() {
            record.add(UndoEntry::Rule(rule));
        }
        for (object, key) in WfpKeys::derive(&n).all() {
            record.add(UndoEntry::Wfp { object, key });
        }
        let group_sid = "S-1-5-21-1-2-3-1100";
        observed.group_sid = Some(group_sid.into());
        for key in AWKWARD_KEYS {
            let Some(path) = key.strip_prefix("remove-closed:") else {
                continue;
            };
            observed.closable.insert(path.to_owned());
            record.add(UndoEntry::Closed {
                sid: group_sid.into(),
                path: path.to_owned(),
                mask: CLOSED_FOLDER_DENY,
                flags: CLOSED_FOLDER_FLAGS,
            });
        }
        let remove = plan_remove(&n, &record, &observed, &|_| Some(false)).unwrap();
        assert_eq!(remove.len(), record.entries.len());
        for step in &remove {
            assert_eq!(step.key(), undo_key(&step.entry));
            keys.push(step.key());
        }
        // The keys of the elevated step's own failures.
        for key in [
            "names", "plan", "record", "password", "pipe", "observe", "remove",
        ] {
            keys.push(key.to_owned());
        }

        let nonce = nonce();
        let mut r = Receiver::new();
        assert_eq!(
            r.take(
                &Message::Hello {
                    nonce: nonce.clone()
                }
                .encode(),
                &nonce
            )
            .unwrap(),
            Event::Started
        );
        let outcomes = [
            StepOutcome::Created,
            StepOutcome::Updated,
            StepOutcome::Verified,
            StepOutcome::Removed,
            StepOutcome::AlreadyGone,
            StepOutcome::Partial,
        ];
        for key in &keys {
            for outcome in outcomes {
                let line = Message::Step {
                    key: key.clone(),
                    outcome,
                }
                .encode();
                assert_eq!(
                    r.take(&line, &nonce).unwrap(),
                    Event::Step {
                        key: key.clone(),
                        outcome
                    },
                    "{line:?}"
                );
            }
            let line = Message::Note {
                key: key.clone(),
                message: format!("not removed: Access is denied.\r\n{key}"),
            }
            .encode();
            assert!(
                matches!(r.take(&line, &nonce).unwrap(), Event::Note { key: k, .. } if k == *key),
                "{line:?}"
            );
        }
        // Every DNS filter Windows can refuse reaches the owner as a gap,
        // with its reason on one line, and the receiver keeps each.
        for object in super::super::wfp::WfpObject::FILTERS {
            let line = Message::Gap {
                key: object.key().to_owned(),
                message: "WFP error 0x80320028 (FWP_E_OUT_OF_BOUNDS);\r\nprobe: x".to_owned(),
            }
            .encode();
            assert_eq!(
                r.take(&line, &nonce).unwrap(),
                Event::Gap {
                    key: object.key().to_owned(),
                    message: "WFP error 0x80320028 (FWP_E_OUT_OF_BOUNDS);  probe: x".to_owned(),
                },
                "{line:?}"
            );
        }
        assert_eq!(
            r.gaps()
                .iter()
                .map(|(k, _)| k.as_str())
                .collect::<Vec<&str>>(),
            super::super::wfp::WfpObject::FILTERS
                .iter()
                .map(|o| o.key())
                .collect::<Vec<&str>>()
        );
        for role in n.roles() {
            let line = Message::Password {
                role,
                password: Password::generate(&n.password_forbidden_parts(role)).unwrap(),
            }
            .encode();
            assert_eq!(
                r.take(&line, &nonce).unwrap(),
                Event::PasswordReceived(role)
            );
        }
        assert_eq!(
            r.take(&Message::End.encode(), &nonce).unwrap(),
            Event::Finished
        );
        assert!(r.unparsed().is_empty());
        assert_eq!(r.into_passwords().unwrap().len(), n.roles().len());

        // A failure, with each key and with what a setup step's failure
        // carries, ends a run on the owner's side.
        for key in &keys {
            let mut r = Receiver::new();
            r.take(
                &Message::Hello {
                    nonce: nonce.clone(),
                }
                .encode(),
                &nonce,
            )
            .unwrap();
            let leaves = StepId::from_key(key)
                .map(failure_leaves)
                .unwrap_or_default();
            let line = Message::Fail {
                key: key.clone(),
                message: format!("I/O error at C:\\: in use (os error 32); {leaves}"),
            }
            .encode();
            assert!(
                matches!(r.take(&line, &nonce).unwrap(), Event::Failed { key: k, .. } if k == *key),
                "{line:?}"
            );
            assert!(r.failed());
        }
    }

    /// A line the owner's side cannot read, once the run is known to be this
    /// one's, is named and read past, so the elevated step goes on removing;
    /// the run still fails. Before the greeting nothing is read past.
    #[test]
    fn an_unreadable_line_is_named_read_past_and_fails_the_run() {
        let n = nonce();
        let raw = "step remove-closed:C:\\Users\\Public removed\n";
        let mut r = Receiver::new();
        assert_eq!(
            r.take(raw, &n).unwrap_err(),
            HandshakeError::Malformed,
            "nothing is read past before the greeting"
        );
        let mut r = Receiver::new();
        r.take(&Message::Hello { nonce: n.clone() }.encode(), &n)
            .unwrap();
        // The line an elevated step that wrote raw paths sent.
        assert_eq!(
            r.take(raw, &n).unwrap(),
            Event::Unparsed(r"step remove-closed:C:\Users\Public".into())
        );
        assert!(matches!(
            r.take("step remove-hidden:kgraf-on-1a2b3c4d removed\n", &n)
                .unwrap(),
            Event::Step { .. }
        ));
        // A password line is named by its kind alone.
        let secret = "password offline not-a-valid-password-at-all";
        let Event::Unparsed(name) = r.take(secret, &n).unwrap() else {
            panic!("a bad password line is read past");
        };
        assert_eq!(name, "password");
        assert_eq!(
            r.take("bogus words here\n", &n).unwrap(),
            Event::Unparsed("a line of no known kind".into())
        );
        assert_eq!(r.take("end\n", &n).unwrap(), Event::Finished);
        assert!(r.finished());
        assert_eq!(r.unparsed().len(), 3);
        assert_eq!(r.into_passwords().unwrap_err(), HandshakeError::Malformed);
        // A wrong order still ends the run at once.
        let mut r = Receiver::new();
        r.take(&Message::Hello { nonce: n.clone() }.encode(), &n)
            .unwrap();
        assert_eq!(
            r.take(&Message::Hello { nonce: n.clone() }.encode(), &n)
                .unwrap_err(),
            HandshakeError::Order
        );
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
