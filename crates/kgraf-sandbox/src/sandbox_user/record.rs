//! The undo record: what the setup made, so `remove` undoes exactly that.
//!
//! The elevated step writes it after every step that changes the machine, so
//! a setup that fails half way still lists what it made. It lives where only
//! administrators can write (`%ProgramData%\kGraf\sandbox`). `remove` still
//! checks each entry against the names it derives from the owner's SID, so a
//! changed record can only make `remove` skip something, never remove a
//! user or rule kGraf did not make.
//!
//! Plain text, one entry per line:
//!
//! ```text
//! kgraf-sandbox-record 1
//! owner S-1-5-21-...-1001
//! tag -
//! user kgraf-off-1a2b3c4d
//! group kgraf-sandbox-1a2b3c4d
//! rights kgraf-off-1a2b3c4d SeDenyNetworkLogonRight,SeDenyBatchLogonRight
//! hidden kgraf-off-1a2b3c4d
//! rule kgraf-sandbox-1a2b3c4d-block-outbound
//! closed S-1-5-21-...-1003 0xd0156 0x0 C:\ProgramData
//! wfp-provider 3c0e5a71-2b9d-8f40-a1c3-5d6e7f809a1b
//! ```
//!
//! A `closed` line names a folder where setup denied the sandbox group the
//! right to create or change entries, the group's SID, and the entry's own
//! access mask and flags, so `remove` can take exactly that entry off again
//! even after the group is gone and even after a later version of kGraf
//! writes a different mask. Removal matches the mask and the flags exactly:
//! it never takes off an entry wider than the one it wrote.

use super::acl::{ACE_INHERITANCE_FLAGS, WRITE_RIGHTS};
use super::names::is_sid_string;
use super::wfp::{WfpObject, guid_text, parse_guid};

pub const RECORD_HEADER: &str = "kgraf-sandbox-record 1";

/// The logon rights the setup denies each sandbox user. Interactive logon
/// stays: the secondary logon service (`CreateProcessWithLogonW`, which a
/// standard user calls to start a command as the sandbox user) performs an
/// interactive logon. The users are hidden from the sign-in screen instead,
/// and their passwords are random and stored only for the owner.
pub const DENIED_LOGON_RIGHTS: [&str; 4] = [
    "SeDenyRemoteInteractiveLogonRight",
    "SeDenyNetworkLogonRight",
    "SeDenyBatchLogonRight",
    "SeDenyServiceLogonRight",
];

/// One thing the setup made.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum UndoEntry {
    User(String),
    Group(String),
    Rights {
        user: String,
        rights: Vec<String>,
    },
    /// The user's value under Winlogon's `SpecialAccounts\UserList`.
    Hidden(String),
    Rule(String),
    /// A deny entry for the sandbox group (`sid`) on the folder `path`
    /// alone, so no sandbox user can create or change entries there, with
    /// the entry's own access mask and flags: removal matches on those, not
    /// on the constants of the version that runs it.
    Closed {
        sid: String,
        path: String,
        mask: u32,
        flags: u8,
    },
    /// A WFP provider, sublayer or filter, by its key.
    Wfp {
        object: WfpObject,
        key: u128,
    },
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RecordError {
    #[error("the undo record does not start with {RECORD_HEADER:?}")]
    Header,
    #[error("the undo record line {line} is not understood: {text:?}")]
    Line { line: usize, text: String },
    #[error("the undo record has no owner line")]
    NoOwner,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub owner_sid: String,
    pub tag: Option<String>,
    pub entries: Vec<UndoEntry>,
}

impl Record {
    pub fn new(owner_sid: &str, tag: Option<&str>) -> Self {
        Self {
            owner_sid: owner_sid.to_owned(),
            tag: tag.map(str::to_owned),
            entries: Vec::new(),
        }
    }

    /// Adds an entry once. Rights for a user already listed are merged.
    pub fn add(&mut self, entry: UndoEntry) {
        if let UndoEntry::Rights { user, rights } = &entry
            && let Some(UndoEntry::Rights { rights: have, .. }) = self
                .entries
                .iter_mut()
                .find(|e| matches!(e, UndoEntry::Rights { user: u, .. } if u == user))
        {
            for r in rights {
                if !have.contains(r) {
                    have.push(r.clone());
                }
            }
            return;
        }
        if !self.entries.contains(&entry) {
            self.entries.push(entry);
        }
    }

    pub fn remove(&mut self, entry: &UndoEntry) {
        self.entries.retain(|e| e != entry);
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn to_text(&self) -> String {
        let mut out = format!(
            "{RECORD_HEADER}\nowner {}\ntag {}\n",
            self.owner_sid,
            self.tag.as_deref().unwrap_or("-")
        );
        for e in &self.entries {
            let line = match e {
                UndoEntry::User(n) => format!("user {n}"),
                UndoEntry::Group(n) => format!("group {n}"),
                UndoEntry::Rights { user, rights } => format!("rights {user} {}", rights.join(",")),
                UndoEntry::Hidden(n) => format!("hidden {n}"),
                UndoEntry::Rule(n) => format!("rule {n}"),
                UndoEntry::Closed {
                    sid,
                    path,
                    mask,
                    flags,
                } => format!("closed {sid} {mask:#x} {flags:#x} {path}"),
                UndoEntry::Wfp { object, key } => format!("{} {}", object.key(), guid_text(*key)),
            };
            out.push_str(&line);
            out.push('\n');
        }
        out
    }

    pub fn parse(text: &str) -> Result<Self, RecordError> {
        let mut lines = text.lines().enumerate();
        match lines.next() {
            Some((_, l)) if l.trim_end() == RECORD_HEADER => {}
            _ => return Err(RecordError::Header),
        }
        let mut owner = None;
        let mut tag = None;
        let mut entries = Vec::new();
        for (i, raw) in lines {
            let line = raw.trim_end();
            if line.is_empty() {
                continue;
            }
            let bad = || RecordError::Line {
                line: i + 1,
                text: line.to_owned(),
            };
            let (kind, rest) = line.split_once(' ').ok_or_else(bad)?;
            if rest.is_empty() || rest.chars().any(char::is_control) {
                return Err(bad());
            }
            let one_word = |s: &str| (!s.contains(' ')).then(|| s.to_owned()).ok_or_else(bad);
            match kind {
                "owner" if is_sid_string(rest) => owner = Some(rest.to_owned()),
                "tag" => tag = (rest != "-").then(|| rest.to_owned()),
                "user" => entries.push(UndoEntry::User(one_word(rest)?)),
                "group" => entries.push(UndoEntry::Group(one_word(rest)?)),
                "hidden" => entries.push(UndoEntry::Hidden(one_word(rest)?)),
                "rule" => entries.push(UndoEntry::Rule(one_word(rest)?)),
                "closed" => {
                    let (sid, rest) = rest.split_once(' ').ok_or_else(bad)?;
                    let (mask, rest) = rest.split_once(' ').ok_or_else(bad)?;
                    let (flags, path) = rest.split_once(' ').ok_or_else(bad)?;
                    let hex = |s: &str| u32::from_str_radix(s.strip_prefix("0x")?, 16).ok();
                    let (Some(mask), Some(flags)) = (hex(mask), hex(flags)) else {
                        return Err(bad());
                    };
                    let flags = u8::try_from(flags).map_err(|_| bad())?;
                    // The mask must be write rights alone and not empty, and
                    // the flags must carry no inheritance: removal takes the
                    // entry the line describes off a folder, so a line that
                    // described a read deny or an inherited entry would make
                    // removal change more than setup wrote.
                    if !is_sid_string(sid)
                        || path.trim().is_empty()
                        || mask == 0
                        || mask & !WRITE_RIGHTS != 0
                        || flags & ACE_INHERITANCE_FLAGS != 0
                    {
                        return Err(bad());
                    }
                    entries.push(UndoEntry::Closed {
                        sid: sid.to_owned(),
                        path: path.to_owned(),
                        mask,
                        flags,
                    });
                }
                "rights" => {
                    let (user, list) = rest.split_once(' ').ok_or_else(bad)?;
                    let rights: Vec<String> = list.split(',').map(str::to_owned).collect();
                    if rights
                        .iter()
                        .any(|r| !DENIED_LOGON_RIGHTS.contains(&r.as_str()))
                    {
                        return Err(bad());
                    }
                    entries.push(UndoEntry::Rights {
                        user: one_word(user)?,
                        rights,
                    });
                }
                k => {
                    let object = WfpObject::from_key(k).ok_or_else(bad)?;
                    let key = parse_guid(rest).ok_or_else(bad)?;
                    entries.push(UndoEntry::Wfp { object, key });
                }
            }
        }
        Ok(Self {
            owner_sid: owner.ok_or(RecordError::NoOwner)?,
            tag,
            entries,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Record {
        let mut r = Record::new("S-1-5-21-1-2-3-1001", None);
        r.add(UndoEntry::User("kgraf-off-1a2b3c4d".into()));
        r.add(UndoEntry::Group("kgraf-sandbox-1a2b3c4d".into()));
        r.add(UndoEntry::Rights {
            user: "kgraf-off-1a2b3c4d".into(),
            rights: vec![DENIED_LOGON_RIGHTS[0].into()],
        });
        r.add(UndoEntry::Hidden("kgraf-off-1a2b3c4d".into()));
        r.add(UndoEntry::Rule(
            "kgraf-sandbox-1a2b3c4d-block-outbound".into(),
        ));
        r.add(UndoEntry::Closed {
            sid: "S-1-5-21-1-2-3-1003".into(),
            path: r"C:\Users\Public\Public Documents".into(),
            mask: super::super::acl::CLOSED_FOLDER_DENY,
            flags: super::super::acl::CLOSED_FOLDER_FLAGS,
        });
        for object in WfpObject::ALL {
            r.add(UndoEntry::Wfp {
                object,
                key: 0x3c0e5a71_2b9d_8f40_a1c3_5d6e7f809a10 + object as u128,
            });
        }
        r
    }

    #[test]
    fn a_record_round_trips() {
        let r = sample();
        assert_eq!(Record::parse(&r.to_text()).unwrap(), r);
        let mut tagged = Record::new("S-1-5-18", Some("wintest"));
        tagged.add(UndoEntry::User("kgraf-on-00000000".into()));
        assert_eq!(Record::parse(&tagged.to_text()).unwrap(), tagged);
        assert!(r.to_text().starts_with(RECORD_HEADER));
        // The WFP keys are written in the usual GUID text form, one line each.
        let text = r.to_text();
        for object in WfpObject::ALL {
            assert_eq!(
                text.lines()
                    .filter(|l| l.starts_with(&format!("{} ", object.key())))
                    .count(),
                1,
                "{text}"
            );
        }
        assert!(text.contains("wfp-provider 3c0e5a71-2b9d-8f40-a1c3-5d6e7f809a10\n"));
    }

    #[test]
    fn add_is_idempotent_and_merges_rights() {
        let mut r = sample();
        let before = r.entries.len();
        r.add(UndoEntry::User("kgraf-off-1a2b3c4d".into()));
        r.add(UndoEntry::Rights {
            user: "kgraf-off-1a2b3c4d".into(),
            rights: vec![DENIED_LOGON_RIGHTS[0].into(), DENIED_LOGON_RIGHTS[1].into()],
        });
        assert_eq!(r.entries.len(), before);
        let rights = r
            .entries
            .iter()
            .find_map(|e| match e {
                UndoEntry::Rights { rights, .. } => Some(rights.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(rights, vec![DENIED_LOGON_RIGHTS[0], DENIED_LOGON_RIGHTS[1]]);
        r.remove(&UndoEntry::User("kgraf-off-1a2b3c4d".into()));
        assert_eq!(r.entries.len(), before - 1);
    }

    /// A `closed` line carries the mask and the flags of the entry setup
    /// wrote, not the constants of the version that reads it. A version that
    /// denies more rights later must neither orphan an entry an earlier one
    /// wrote nor take off a wider entry while deleting its line.
    #[test]
    fn a_closed_line_carries_the_entrys_own_mask_and_flags() {
        let line = |mask: u32| {
            let mut r = Record::new("S-1-5-21-1-2-3-1001", None);
            r.add(UndoEntry::Closed {
                sid: "S-1-5-21-1-2-3-1003".into(),
                path: r"C:\ProgramData".into(),
                mask,
                flags: 0,
            });
            let text = r.to_text();
            assert_eq!(Record::parse(&text).unwrap(), r, "{text}");
            text.lines()
                .find(|l| l.starts_with("closed "))
                .unwrap()
                .to_owned()
        };
        // Today's mask, and a narrower one an earlier version could have
        // written: each comes back as it went in.
        assert_eq!(
            line(super::super::acl::CLOSED_FOLDER_DENY),
            r"closed S-1-5-21-1-2-3-1003 0xd0156 0x0 C:\ProgramData"
        );
        assert_eq!(
            line(0x0000_0116),
            r"closed S-1-5-21-1-2-3-1003 0x116 0x0 C:\ProgramData"
        );
    }

    #[test]
    fn damaged_records_are_refused() {
        assert_eq!(Record::parse(""), Err(RecordError::Header));
        assert_eq!(Record::parse("something else\n"), Err(RecordError::Header));
        assert_eq!(
            Record::parse(&format!("{RECORD_HEADER}\ntag -\n")),
            Err(RecordError::NoOwner)
        );
        for bad in [
            "owner not-a-sid",
            "user two words",
            "delete C:\\Windows",
            "rights kgraf-off-1 SeDebugPrivilege",
            "rule",
            "user ",
            "closed S-1-5-21-1-2-3-1003",
            "closed S-1-5-21-1-2-3-1003 C:\\ProgramData",
            "closed not-a-sid 0xd0156 0x0 C:\\ProgramData",
            "closed S-1-5-21-1-2-3-1003 0xd0156 0x0  ",
            // Not a hexadecimal mask, an empty mask, a read right in the
            // mask, and an inherited or inheritable entry.
            "closed S-1-5-21-1-2-3-1003 d0116 0x0 C:\\ProgramData",
            "closed S-1-5-21-1-2-3-1003 0x0 0x0 C:\\ProgramData",
            "closed S-1-5-21-1-2-3-1003 0xd01a9 0x0 C:\\ProgramData",
            "closed S-1-5-21-1-2-3-1003 0xd0156 0x10 C:\\ProgramData",
            "closed S-1-5-21-1-2-3-1003 0xd0156 0x3 C:\\ProgramData",
            "closed S-1-5-21-1-2-3-1003 0xd0156 0x100 C:\\ProgramData",
            "wfp-filter not-a-guid",
            "wfp-filter 3c0e5a71-2b9d-8f40-a1c3-5d6e7f809a1b extra",
            "wfp-callout 3c0e5a71-2b9d-8f40-a1c3-5d6e7f809a1b",
        ] {
            let text = format!("{RECORD_HEADER}\nowner S-1-5-18\n{bad}\n");
            assert!(
                matches!(Record::parse(&text), Err(RecordError::Line { .. })),
                "{bad:?} must be refused"
            );
        }
    }
}
