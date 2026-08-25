//! Public-boundary sanitation required by Architecture v2 part 2 §10.1.

/// Returns whether `character` is trimmed by ECMAScript string operations.
///
/// This is the union of ECMAScript's `WhiteSpace` and `LineTerminator`
/// productions. It intentionally includes U+FEFF and excludes U+0085, unlike
/// Rust's [`char::is_whitespace`] predicate.
pub fn is_ecmascript_whitespace(character: char) -> bool {
    matches!(
        character,
        '\u{0009}'
            | '\u{000a}'
            | '\u{000b}'
            | '\u{000c}'
            | '\u{000d}'
            | '\u{0020}'
            | '\u{00a0}'
            | '\u{1680}'
            | '\u{2000}'
            ..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
    )
}

/// Trims the same leading and trailing characters as ECMAScript
/// `String.prototype.trim()`.
pub fn trim_ecmascript(value: &str) -> &str {
    value.trim_matches(is_ecmascript_whitespace)
}

/// Replacement used when secret-bearing error data reaches a public boundary.
const REDACTED: &str = "[REDACTED]";

/// Removes unpaired UTF-16 surrogate code units while preserving valid pairs.
///
/// Native Rust strings are already valid Unicode scalar values and therefore
/// cannot contain lone surrogates. This function is the explicit boundary for
/// JavaScript, UTF-16 FFI, and compatibility inputs that can contain them. It
/// matches pinned Pi's `sanitizeSurrogates`: lone high and low surrogates are
/// removed rather than replaced, while valid pairs decode normally.
pub fn sanitize_utf16_surrogates(units: &[u16]) -> String {
    let mut output = String::with_capacity(units.len());
    let mut index = 0;

    while let Some(&unit) = units.get(index) {
        if (0xD800..=0xDBFF).contains(&unit) {
            let Some(&low) = units.get(index + 1) else {
                break;
            };
            if (0xDC00..=0xDFFF).contains(&low) {
                let scalar =
                    0x1_0000 + ((u32::from(unit) - 0xD800) << 10) + (u32::from(low) - 0xDC00);
                output.push(char::from_u32(scalar).expect("a valid surrogate pair is a scalar"));
                index += 2;
                continue;
            }
        } else if !(0xDC00..=0xDFFF).contains(&unit) {
            output.push(char::from_u32(u32::from(unit)).expect("a non-surrogate u16 is a scalar"));
        }

        index += 1;
    }

    output
}

pub(crate) fn redact_public_text(text: String, secret_values: &[&str]) -> String {
    let mut secrets = secret_values
        .iter()
        .copied()
        .filter(|secret| !secret.is_empty() && *secret != REDACTED)
        .collect::<Vec<_>>();
    secrets.sort_unstable_by_key(|secret| std::cmp::Reverse(secret.len()));
    secrets.dedup();

    let mut redacted = text;
    for secret in secrets {
        redacted = redacted.replace(secret, REDACTED);
    }
    redact_named_values(&redacted)
}

const SENSITIVE_KEYS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "cf-aig-authorization",
    "x-api-key",
    "x-goog-api-key",
    "api-key",
    "api_key",
    "apikey",
    "access-token",
    "access_token",
    "accesstoken",
    "refresh-token",
    "refresh_token",
    "refreshtoken",
    "id-token",
    "id_token",
    "client-secret",
    "client_secret",
    "password",
    "cookie",
    "set-cookie",
];

fn redact_named_values(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut cursor = 0;

    while let Some((_key_start, value_start, value_end)) = next_sensitive_value(input, cursor) {
        output.push_str(&input[cursor..value_start]);
        output.push_str(REDACTED);
        cursor = value_end;
    }
    output.push_str(&input[cursor..]);
    output
}

fn next_sensitive_value(input: &str, from: usize) -> Option<(usize, usize, usize)> {
    let bytes = input.as_bytes();

    for key_start in from..bytes.len() {
        for key in SENSITIVE_KEYS {
            let key_bytes = key.as_bytes();
            let key_end = key_start.checked_add(key_bytes.len())?;
            if key_end > bytes.len()
                || !bytes[key_start..key_end].eq_ignore_ascii_case(key_bytes)
                || !is_key_boundary(bytes.get(key_start.wrapping_sub(1)).copied())
                || !is_key_boundary(bytes.get(key_end).copied())
            {
                continue;
            }

            let mut separator = key_end;
            if bytes.get(separator) == Some(&b'\"') || bytes.get(separator) == Some(&b'\'') {
                separator += 1;
            }
            while bytes.get(separator).is_some_and(u8::is_ascii_whitespace) {
                separator += 1;
            }
            if !matches!(bytes.get(separator), Some(b':') | Some(b'=')) {
                continue;
            }

            let mut value_start = separator + 1;
            while bytes.get(value_start).is_some_and(u8::is_ascii_whitespace) {
                value_start += 1;
            }
            let Some(&first) = bytes.get(value_start) else {
                continue;
            };

            if first == b'\"' || first == b'\'' {
                let quote = first;
                let content_start = value_start + 1;
                let mut value_end = content_start;
                while let Some(&candidate) = bytes.get(value_end) {
                    if candidate == quote && !is_escaped(bytes, value_end) {
                        return Some((key_start, content_start, value_end));
                    }
                    value_end += 1;
                }
                return Some((key_start, content_start, bytes.len()));
            }

            let mut value_end = value_start;
            while let Some(&candidate) = bytes.get(value_end) {
                if candidate.is_ascii_whitespace() || matches!(candidate, b',' | b';' | b'}' | b']')
                {
                    break;
                }
                value_end += 1;
            }
            if value_end > value_start {
                return Some((key_start, value_start, value_end));
            }
        }
    }

    None
}

fn is_key_boundary(byte: Option<u8>) -> bool {
    byte.is_none_or(|byte| !byte.is_ascii_alphanumeric() && byte != b'_' && byte != b'-')
}

fn is_escaped(bytes: &[u8], index: usize) -> bool {
    let mut backslashes = 0;
    let mut cursor = index;
    while cursor > 0 && bytes[cursor - 1] == b'\\' {
        backslashes += 1;
        cursor -= 1;
    }
    backslashes % 2 == 1
}
