//! Plain text and code extraction: read the file and require valid UTF-8.
//!
//! No section structure is inferred here. Code's structure belongs to the
//! code graph (`kgraf-graph`), not to this crate; a document walker that
//! also parsed function boundaries would duplicate that indexer.

use std::path::Path;

use crate::error::IngestError;

pub fn extract(path: &Path) -> Result<String, IngestError> {
    let bytes = std::fs::read(path).map_err(|source| IngestError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    String::from_utf8(bytes).map_err(|_| IngestError::InvalidUtf8 {
        path: path.to_path_buf(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_utf8_text() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("notes.txt");
        std::fs::write(&path, "plain text body").expect("write fixture");
        assert_eq!(extract(&path).expect("extract"), "plain text body");
    }

    #[test]
    fn rejects_non_utf8_bytes_instead_of_returning_garbage() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("binary.txt");
        std::fs::write(&path, [0xff, 0xfe, 0x00, 0xff]).expect("write fixture");
        let err = extract(&path).expect_err("invalid utf-8 must error");
        assert!(matches!(err, IngestError::InvalidUtf8 { .. }));
    }
}
