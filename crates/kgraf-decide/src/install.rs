//! Fetching the model on first run.
//!
//! The model is **not bundled**. A checkpoint in this class is a few hundred megabytes
//! against a Tauri application's usual tens, and shipping it would triple the download
//! for a component a user may decline. So it is fetched on first use, once, into a cache
//! beside the project's other local state.
//!
//! Four rules, each of which is a failure this project has already written a rule about.
//!
//! **The digest is required and is verified.** A manifest without a checksum is refused
//! at construction rather than downloaded hopefully. A model that arrives with the wrong
//! digest is deleted and the installation fails loudly: a corrupt decision model does not
//! error, it produces confident garbage, which is the worst possible failure for something
//! whose entire output is a probability.
//!
//! **The digest is not invented.** This module ships **no built-in manifest**, because
//! nobody has verified one from inside this repository. A fabricated checksum would either
//! reject the real file forever or, worse, be replaced by whatever hash the first download
//! happened to produce, which is trust-on-first-use dressed as verification. The manifest
//! is supplied by whoever can check it against the publisher.
//!
//! **A declined install stays declined.** Recorded, so the offer is made once. The
//! difference between encouraging and nagging is whether the answer is remembered.
//!
//! **The license travels with the weights.** A manifest names the model's license, because
//! kGraf is redistributing nothing but is asking a user to fetch something, and at
//! least one candidate model in this category publishes no license at all, which makes it
//! unusable here regardless of its benchmarks.

use crate::availability::Absence;

/// Where a model comes from and how to know it arrived intact.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ModelManifest {
    /// Stable short name, used for the cache directory.
    pub id: String,
    /// Human-readable name, for the install prompt.
    pub display_name: String,
    /// Where to fetch it.
    pub url: String,
    /// Lowercase hex SHA-256 of the file. Required.
    pub sha256: String,
    /// Expected size in bytes, checked before hashing so a truncated download fails fast
    /// and a wildly wrong URL does not stream for an hour.
    pub size_bytes: u64,
    /// SPDX identifier of the **model weights'** license, which is frequently different
    /// from the serving code's.
    pub license: String,
    /// The largest state this model can read, in estimated tokens.
    pub state_token_limit: u64,
}

/// Why a manifest is unusable.
#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    /// The digest is absent or not a SHA-256.
    ///
    /// Refused at construction. A download without a digest to check is a download that
    /// cannot fail, and a decision model that silently arrived corrupt emits confident
    /// nonsense rather than an error.
    #[error("manifest `{id}` has no usable sha256 digest; refusing to fetch unverifiable weights")]
    NoDigest { id: String },
    /// No license recorded for the weights.
    ///
    /// Refused, because at least one model in this category publishes none, which makes it
    /// legally unusable here whatever its accuracy.
    #[error("manifest `{id}` records no license for the weights; refusing to fetch")]
    NoLicense { id: String },
    /// The URL is not https.
    #[error("manifest `{id}` fetches over `{scheme}`; weights must come over https")]
    InsecureUrl { id: String, scheme: String },
    /// A zero or absurd size.
    #[error("manifest `{id}` declares an implausible size of {size_bytes} bytes")]
    ImplausibleSize { id: String, size_bytes: u64 },
    /// The id names the cache directory, so it must be one plain path component.
    #[error(
        "manifest id `{id}` is not a plain name (letters, digits, `.`, `_`, `-`; no separators)"
    )]
    BadId { id: String },
}

impl ModelManifest {
    /// Check a manifest before anything is fetched.
    pub fn validate(&self) -> Result<(), ManifestError> {
        // The id is joined into the cache path, and a failed verification deletes
        // the file at that path: a separator or `..` would reach outside the cache.
        let id_ok = !self.id.is_empty()
            && self.id.len() <= 64
            && !self.id.starts_with('.')
            && self
                .id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
        if !id_ok {
            return Err(ManifestError::BadId {
                id: self.id.clone(),
            });
        }
        let digest_ok = self.sha256.len() == 64
            && self.sha256.chars().all(|c| c.is_ascii_hexdigit())
            && self.sha256.chars().all(|c| !c.is_ascii_uppercase());
        if !digest_ok {
            return Err(ManifestError::NoDigest {
                id: self.id.clone(),
            });
        }
        if self.license.trim().is_empty() {
            return Err(ManifestError::NoLicense {
                id: self.id.clone(),
            });
        }
        if !self.url.starts_with("https://") {
            let scheme = self
                .url
                .split_once("://")
                .map(|(scheme, _)| scheme.to_owned())
                .unwrap_or_else(|| "none".to_owned());
            return Err(ManifestError::InsecureUrl {
                id: self.id.clone(),
                scheme,
            });
        }
        // A model in this class is tens of megabytes at the smallest. Anything under a
        // megabyte is a stub, an error page, or a redirect saved to disk.
        if self.size_bytes < 1_000_000 || self.size_bytes > 100_000_000_000 {
            return Err(ManifestError::ImplausibleSize {
                id: self.id.clone(),
                size_bytes: self.size_bytes,
            });
        }
        Ok(())
    }
}

/// What the user has been asked, and answered, about installing a model.
///
/// Stored, so an offer is made once. Encouragement that repeats is nagging.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallChoice {
    /// Never asked.
    Unasked,
    /// Asked and accepted.
    Accepted,
    /// Asked and declined. Not re-asked.
    Declined,
}

/// Whether a model is present on this machine, and what to do about it.
#[derive(Clone, Debug, PartialEq)]
pub struct InstallState {
    pub manifest: ModelManifest,
    /// Path the weights live at when installed.
    pub path: std::path::PathBuf,
    pub installed: bool,
    pub choice: InstallChoice,
}

impl InstallState {
    /// Inspect a cache directory for this manifest.
    ///
    /// Presence is a file of the **right size** at the right path. A full hash on every
    /// startup would read hundreds of megabytes for a check that only matters after a
    /// fetch, and a size check catches the realistic corruption, a truncated or
    /// interrupted download.
    ///
    /// A manifest whose id is not a plain name is refused, since the id names the
    /// directory under `cache_dir`.
    pub fn inspect(
        manifest: ModelManifest,
        cache_dir: &std::path::Path,
        choice: InstallChoice,
    ) -> Result<Self, ManifestError> {
        manifest.validate()?;
        let path = cache_dir.join(&manifest.id).join("weights.bin");
        let installed = std::fs::metadata(&path)
            .map(|meta| meta.is_file() && meta.len() == manifest.size_bytes)
            .unwrap_or(false);
        Ok(Self {
            manifest,
            path,
            installed,
            choice,
        })
    }

    /// The absence this state implies, or `None` when the model is present.
    pub fn absence(&self) -> Option<Absence> {
        if self.installed {
            return None;
        }
        Some(match self.choice {
            InstallChoice::Declined => Absence::Declined,
            InstallChoice::Unasked | InstallChoice::Accepted => Absence::NotInstalled,
        })
    }

    /// Whether the interface should offer to install.
    pub fn should_offer(&self) -> bool {
        !self.installed && self.choice == InstallChoice::Unasked
    }

    /// The offer text: what it is, what it costs, and that it is optional.
    ///
    /// Honest about the download size, because a user who finds out afterwards is a user
    /// who was misled, and explicit that declining is fine, because it is.
    pub fn offer(&self) -> String {
        format!(
            "kGraf can install {} ({:.0} MB, {}) to judge scheduling and routing \
             choices locally. It runs on the CPU, nothing leaves this machine, and it is \
             optional: without it those choices are made by fixed rules and every one of \
             them is labelled as such.",
            self.manifest.display_name,
            self.manifest.size_bytes as f64 / 1_000_000.0,
            self.manifest.license,
        )
    }
}

/// Verify a downloaded file against its manifest, deleting it if it does not match.
///
/// Deleting rather than leaving it is deliberate: a file that failed verification will
/// otherwise be found by the next `inspect` and treated as installed if its size happens
/// to match, and a corrupt decision model produces confident nonsense rather than errors.
pub fn verify_and_keep(
    manifest: &ModelManifest,
    path: &std::path::Path,
) -> Result<(), VerifyError> {
    use std::io::Read;
    let unreadable = |source| VerifyError::Unreadable {
        path: path.display().to_string(),
        source,
    };
    // The size is judged from metadata first: a wrong-sized file is never read.
    let actual = std::fs::metadata(path).map_err(unreadable)?.len();
    if actual != manifest.size_bytes {
        let _ = std::fs::remove_file(path);
        return Err(VerifyError::WrongSize {
            expected: manifest.size_bytes,
            actual,
        });
    }
    // Hashed in blocks, so memory stays flat for a multi-gigabyte model. The read
    // is capped one byte past the declared size, so a file that grows while it
    // is read is caught as a wrong size, not hashed without bound.
    let file = std::fs::File::open(path).map_err(unreadable)?;
    let mut reader = file.take(manifest.size_bytes + 1);
    let mut hasher = Sha256::new();
    let mut block = vec![0u8; 1 << 20];
    let mut read_total = 0u64;
    loop {
        let n = reader.read(&mut block).map_err(unreadable)?;
        if n == 0 {
            break;
        }
        read_total += n as u64;
        hasher.update(&block[..n]);
    }
    if read_total != manifest.size_bytes {
        let _ = std::fs::remove_file(path);
        return Err(VerifyError::WrongSize {
            expected: manifest.size_bytes,
            actual: read_total,
        });
    }
    let digest = hasher.finish_hex();
    if digest != manifest.sha256 {
        let _ = std::fs::remove_file(path);
        return Err(VerifyError::DigestMismatch {
            expected: manifest.sha256.clone(),
            actual: digest,
        });
    }
    Ok(())
}

/// Verification failures.
#[derive(Debug, thiserror::Error)]
pub enum VerifyError {
    #[error("cannot read `{path}`")]
    Unreadable {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("downloaded {actual} bytes, manifest says {expected}; file deleted")]
    WrongSize { expected: u64, actual: u64 },
    #[error("sha256 was {actual}, manifest says {expected}; file deleted")]
    DigestMismatch { expected: String, actual: String },
}

/// Hand-rolled rather than adding a dependency: the whole algorithm is sixty lines, it is
/// used in exactly one place, and a supply-chain dependency in the path that verifies
/// downloads is the wrong trade. Input is consumed in 64-byte blocks as it arrives.
struct Sha256 {
    h: [u32; 8],
    block: [u8; 64],
    filled: usize,
    total_bytes: u64,
}

const SHA256_K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

impl Sha256 {
    fn new() -> Self {
        Self {
            h: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
                0x5be0cd19,
            ],
            block: [0; 64],
            filled: 0,
            total_bytes: 0,
        }
    }

    fn update(&mut self, mut input: &[u8]) {
        self.total_bytes += input.len() as u64;
        while !input.is_empty() {
            let take = (64 - self.filled).min(input.len());
            self.block[self.filled..self.filled + take].copy_from_slice(&input[..take]);
            self.filled += take;
            input = &input[take..];
            if self.filled == 64 {
                let block = self.block;
                self.compress(&block);
                self.filled = 0;
            }
        }
    }

    fn finish_hex(mut self) -> String {
        let bit_len = self.total_bytes * 8;
        // Padding goes through `update`'s buffer without counting toward the length.
        let mut pad = vec![0x80u8];
        while (self.filled + pad.len()) % 64 != 56 {
            pad.push(0);
        }
        pad.extend_from_slice(&bit_len.to_be_bytes());
        let total = self.total_bytes;
        self.update(&pad);
        self.total_bytes = total;
        self.h.iter().map(|word| format!("{word:08x}")).collect()
    }

    fn compress(&mut self, chunk: &[u8; 64]) {
        let mut w = [0u32; 64];
        for (i, word) in chunk.chunks(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let mut v = self.h;
        for i in 0..64 {
            let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
            let ch = (v[4] & v[5]) ^ ((!v[4]) & v[6]);
            let t1 = v[7]
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(SHA256_K[i])
                .wrapping_add(w[i]);
            let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
            let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
            let t2 = s0.wrapping_add(maj);
            v[7] = v[6];
            v[6] = v[5];
            v[5] = v[4];
            v[4] = v[3].wrapping_add(t1);
            v[3] = v[2];
            v[2] = v[1];
            v[1] = v[0];
            v[0] = t1.wrapping_add(t2);
        }
        for (h, v) in self.h.iter_mut().zip(v) {
            *h = h.wrapping_add(v);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SHA-256 as lowercase hex.
    fn sha256_hex(bytes: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        hasher.finish_hex()
    }

    fn manifest() -> ModelManifest {
        ModelManifest {
            id: "example".into(),
            display_name: "An Example Decision Model".into(),
            url: "https://example.invalid/weights.bin".into(),
            sha256: "a".repeat(64),
            size_bytes: 800_000_000,
            license: "Apache-2.0".into(),
            state_token_limit: 8_192,
        }
    }

    #[test]
    fn no_manifest_checksum_is_hardcoded_in_production_code() {
        // The rule worth a test: nobody has verified a digest from inside this repository,
        // and a fabricated one is worse than none. It would either reject the real file
        // forever, or be "corrected" to whatever the first download produced, which is
        // trust-on-first-use wearing verification's clothes.
        //
        // The audit scans only the production half of this file. The test module below
        // legitimately contains 64-character hex: the published SHA-256 known-answer
        // vectors, which are how the hand-rolled implementation is shown to be correct. An
        // earlier version of this test scanned the whole file and therefore flagged those,
        // and the workaround was to split the literals so they would not match, which
        // rustfmt promptly rejoined. Scoping the audit is the real fix; contorting the
        // source to satisfy a test is how a test stops meaning anything.
        let source = include_str!("install.rs");
        let production = source
            .split_once("#[cfg(test)]")
            .map(|(before, _)| before)
            .expect("this file has a test module");
        let invented: Vec<&str> = production
            .split(|c: char| !c.is_ascii_hexdigit())
            .filter(|token| {
                token.len() == 64
                    && token
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
            })
            .collect();
        assert!(
            invented.is_empty(),
            "a 64-character hex literal appears in production code, which would be a \
             checksum somebody invented rather than verified: {invented:?}"
        );
    }

    #[test]
    fn a_manifest_without_a_digest_is_refused_before_anything_is_fetched() {
        let mut bad = manifest();
        bad.sha256 = String::new();
        assert!(matches!(
            bad.validate(),
            Err(ManifestError::NoDigest { .. })
        ));
        bad.sha256 = "not-a-hash".into();
        assert!(matches!(
            bad.validate(),
            Err(ManifestError::NoDigest { .. })
        ));
        // Uppercase is refused too, so a comparison cannot fail on case alone.
        bad.sha256 = "A".repeat(64);
        assert!(matches!(
            bad.validate(),
            Err(ManifestError::NoDigest { .. })
        ));
    }

    #[test]
    fn a_manifest_without_a_weights_license_is_refused() {
        // At least one model in this category publishes no license, which makes it
        // unusable here regardless of its benchmarks.
        let mut bad = manifest();
        bad.license = "  ".into();
        assert!(matches!(
            bad.validate(),
            Err(ManifestError::NoLicense { .. })
        ));
    }

    #[test]
    fn weights_must_come_over_https() {
        let mut bad = manifest();
        bad.url = "http://example.invalid/weights.bin".into();
        assert!(matches!(
            bad.validate(),
            Err(ManifestError::InsecureUrl { .. })
        ));
    }

    #[test]
    fn an_implausible_size_is_refused_so_an_error_page_is_not_mistaken_for_a_model() {
        let mut bad = manifest();
        bad.size_bytes = 4_096;
        assert!(matches!(
            bad.validate(),
            Err(ManifestError::ImplausibleSize { .. })
        ));
    }

    #[test]
    fn a_manifest_id_that_could_leave_the_cache_is_refused() {
        for bad in [
            "",
            "..",
            "../x",
            "a/b",
            "a\\b",
            ".hidden",
            "x y",
            &"a".repeat(65),
        ] {
            let mut m = manifest();
            m.id = bad.to_owned();
            assert!(
                matches!(m.validate(), Err(ManifestError::BadId { .. })),
                "`{bad}` was accepted"
            );
            assert!(
                InstallState::inspect(
                    m,
                    std::path::Path::new("/nonexistent"),
                    InstallChoice::Unasked
                )
                .is_err()
            );
        }
    }

    #[test]
    fn a_valid_manifest_passes() {
        manifest().validate().expect("valid");
    }

    #[test]
    fn sha256_matches_known_vectors() {
        // The hand-rolled implementation is only trustworthy if it is checked against the
        // published vectors.
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn a_streamed_digest_matches_the_one_shot_digest_across_block_edges() {
        for len in [0usize, 1, 55, 56, 57, 63, 64, 65, 119, 120, 128, 1000] {
            let data: Vec<u8> = (0..len).map(|i| (i * 7 + 3) as u8).collect();
            let mut streamed = Sha256::new();
            for piece in data.chunks(13) {
                streamed.update(piece);
            }
            assert_eq!(streamed.finish_hex(), sha256_hex(&data), "length {len}");
        }
        // A million 'a' bytes: the published vector, in uneven pieces.
        let mut hasher = Sha256::new();
        let block = [b'a'; 1000];
        for _ in 0..1000 {
            hasher.update(&block);
        }
        assert_eq!(
            hasher.finish_hex(),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    #[test]
    fn a_file_the_wrong_size_is_never_read() {
        // A path that cannot be read as a file but reports a size: a directory has
        // metadata and a length, and a read would fail, so a wrong-size verdict here
        // proves the size was judged from metadata alone.
        let dir = tempfile::tempdir().expect("tempdir");
        let err = verify_and_keep(&manifest(), dir.path()).expect_err("must reject");
        assert!(
            matches!(err, VerifyError::WrongSize { .. }),
            "read before the size check: {err:?}"
        );
    }

    #[test]
    fn a_corrupt_download_is_rejected_and_deleted() {
        // A corrupt decision model does not error at inference time. It emits confident
        // nonsense, which for something whose whole output is a probability is the worst
        // available failure. So the file does not survive verification.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("weights.bin");
        let contents = vec![7u8; 2_000_000];
        std::fs::write(&path, &contents).expect("write");

        let mut m = manifest();
        m.size_bytes = contents.len() as u64;
        m.sha256 = "b".repeat(64); // deliberately not the real digest
        let err = verify_and_keep(&m, &path).expect_err("must reject");
        assert!(matches!(err, VerifyError::DigestMismatch { .. }));
        assert!(
            !path.exists(),
            "a file that failed verification must not survive"
        );
    }

    #[test]
    fn a_truncated_download_is_rejected_on_size_before_hashing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("weights.bin");
        std::fs::write(&path, vec![0u8; 10]).expect("write");
        let err = verify_and_keep(&manifest(), &path).expect_err("must reject");
        assert!(matches!(err, VerifyError::WrongSize { .. }));
        assert!(!path.exists());
    }

    #[test]
    fn a_correct_download_verifies_and_survives() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("weights.bin");
        let contents = vec![3u8; 1_500_000];
        std::fs::write(&path, &contents).expect("write");
        let mut m = manifest();
        m.size_bytes = contents.len() as u64;
        m.sha256 = sha256_hex(&contents);
        verify_and_keep(&m, &path).expect("verifies");
        assert!(path.exists());
    }

    #[test]
    fn a_missing_model_is_not_installed_and_is_offered_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state =
            InstallState::inspect(manifest(), dir.path(), InstallChoice::Unasked).expect("valid");
        assert!(!state.installed);
        assert!(state.should_offer());
        assert_eq!(state.absence(), Some(Absence::NotInstalled));
    }

    #[test]
    fn a_declined_model_is_not_offered_again() {
        // Encouragement that repeats is nagging. The answer is remembered.
        let dir = tempfile::tempdir().expect("tempdir");
        let state =
            InstallState::inspect(manifest(), dir.path(), InstallChoice::Declined).expect("valid");
        assert!(!state.should_offer());
        assert_eq!(state.absence(), Some(Absence::Declined));
    }

    #[test]
    fn a_truncated_file_on_disk_does_not_count_as_installed() {
        // The realistic corruption: an interrupted download left behind. Reading it as
        // installed would load a broken model.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("example");
        std::fs::create_dir_all(&path).expect("mkdir");
        std::fs::write(path.join("weights.bin"), vec![0u8; 100]).expect("write");
        let state =
            InstallState::inspect(manifest(), dir.path(), InstallChoice::Accepted).expect("valid");
        assert!(!state.installed);
        assert_eq!(state.absence(), Some(Absence::NotInstalled));
    }

    #[test]
    fn the_offer_states_the_size_the_license_and_that_it_is_optional() {
        // A user who learns the download size afterwards was misled.
        let text = InstallState::inspect(
            manifest(),
            std::path::Path::new("/nonexistent"),
            InstallChoice::Unasked,
        )
        .expect("valid")
        .offer();
        assert!(text.contains("800 MB"), "{text}");
        assert!(text.contains("Apache-2.0"), "{text}");
        assert!(text.contains("optional"), "{text}");
        assert!(text.contains("CPU"), "{text}");
        assert!(
            text.contains("nothing leaves this machine"),
            "the local-only property is the reason to accept: {text}"
        );
    }
}
