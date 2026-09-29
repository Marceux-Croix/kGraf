//! Shannon entropy over a byte histogram, the same measure gitleaks gates its
//! own pattern rules on (a rule's `entropy` field in `config/gitleaks.toml`,
//! MIT; see `third_party/gitleaks/UPSTREAM`).
//!
//! **Why this exists.** `generic-api-key` (and several other ported
//! rules) matched ordinary code before this gate existed: `stats.code_tokens =
//! code_spent;` has the shape `token... = value`, and the alphabetic value
//! `code_spent` easily clears the rule's length floor. A real secret's value
//! is close to random over its alphabet; `code_spent` is not. Gitleaks' fix is
//! exactly this: report a `generic-api-key` finding only when the captured
//! value's Shannon entropy meets the rule's configured floor (3.5 bits per
//! byte for `generic-api-key`; see `rules::PatternRule::entropy`).

/// Shannon entropy of `text`, in bits per byte: `-sum(p_i * log2(p_i))` over
/// the byte-value histogram. Case-sensitive and byte-oriented, matching
/// gitleaks' own `math.Log2` computation over the raw secret bytes (not
/// Unicode scalar values; every secret shape here is ASCII, so the two agree).
///
/// `0.0` for an empty string (no bytes, no entropy) rather than `NaN`.
pub fn shannon_entropy(text: &str) -> f64 {
    if text.is_empty() {
        return 0.0;
    }
    let mut counts = [0u32; 256];
    for &b in text.as_bytes() {
        counts[b as usize] += 1;
    }
    let len = text.len() as f64;
    counts
        .iter()
        .filter(|&&c| c > 0)
        .map(|&c| {
            let p = f64::from(c) / len;
            -p * p.log2()
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_single_repeated_byte_has_zero_entropy() {
        assert_eq!(shannon_entropy(""), 0.0);
        assert_eq!(shannon_entropy("aaaaaaaaaa"), 0.0);
    }

    #[test]
    fn an_ordinary_identifier_falls_well_short_of_the_generic_api_key_floor() {
        // The exact false positive from the security review: an assignment
        // whose value is a plain, low-entropy identifier.
        assert!(
            shannon_entropy("code_spent") < 3.5,
            "{}",
            shannon_entropy("code_spent")
        );
    }

    #[test]
    fn a_real_looking_random_token_clears_the_floor() {
        let token = "aK9x2Qm7Lp4Rt8Zv1Ws5Yb3Nc6Hd0Fj";
        assert!(shannon_entropy(token) >= 3.5, "{}", shannon_entropy(token));
    }

    #[test]
    fn uniform_over_128_ascii_values_is_exactly_seven_bits() {
        // `str` must be valid UTF-8, so this stays inside the one-byte-per-char
        // ASCII range (0..128) rather than the full byte range: each of the
        // 128 values appears once, giving a uniform histogram of 128 outcomes,
        // whose entropy is exactly log2(128) = 7 bits.
        let text: String = (0u8..128).map(|b| b as char).collect();
        assert!((shannon_entropy(&text) - 7.0).abs() < 1e-9);
    }
}
