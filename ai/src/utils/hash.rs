//! Deterministic short hashes ⇐ pi `src/utils/hash.ts`.

fn base36(mut value: u32) -> String {
    if value == 0 {
        return "0".to_owned();
    }

    let mut digits = Vec::new();
    while value > 0 {
        let digit = value % 36;
        let byte = if digit < 10 {
            b'0' + u8::try_from(digit).expect("base36 digit is in range")
        } else {
            b'a' + u8::try_from(digit - 10).expect("base36 digit is in range")
        };
        digits.push(char::from(byte));
        value /= 36;
    }
    digits.iter().rev().collect()
}

/// pi's cyrb53-derived `shortHash`, including JavaScript UTF-16 code-unit hashing.
pub fn short_hash(value: &str) -> String {
    let mut h1 = 0xdead_beefu32;
    let mut h2 = 0x41c6_ce57u32;
    for code_unit in value.encode_utf16() {
        h1 = (h1 ^ u32::from(code_unit)).wrapping_mul(2_654_435_761);
        h2 = (h2 ^ u32::from(code_unit)).wrapping_mul(1_597_334_677);
    }
    h1 = (h1 ^ (h1 >> 16)).wrapping_mul(2_246_822_507)
        ^ (h2 ^ (h2 >> 13)).wrapping_mul(3_266_489_909);
    h2 = (h2 ^ (h2 >> 16)).wrapping_mul(2_246_822_507)
        ^ (h1 ^ (h1 >> 13)).wrapping_mul(3_266_489_909);
    format!("{}{}", base36(h2), base36(h1))
}

#[cfg(test)]
mod tests {
    use super::short_hash;

    /// Vectors evaluated by pi `src/utils/hash.ts:2-12` at commit 496185f.
    #[test]
    fn matches_pi_short_hash_vectors() {
        for (input, expected) in [
            ("", "k4n83c7h0j2b"),
            ("a", "m8735310ae7sx"),
            ("hello", "1h6qa0qrowduu"),
            ("call_123|fc_456", "1l8gxfc1027wt9"),
            ("🙈", "kphsz0153ms3q"),
            ("a🙈b", "q0megbstm1j3"),
            ("你好", "603p56zx32gv"),
            (
                "The quick brown fox jumps over the lazy dog",
                "eig47k1th3xf1",
            ),
        ] {
            assert_eq!(short_hash(input), expected, "{input:?}");
        }
    }
}
