//! Lossless ECMAScript string storage.

use serde::de::Visitor;
use serde::ser::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::value::RawValue;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::ops::{AddAssign, Deref};

/// An ECMAScript string: an arbitrary sequence of UTF-16 code units.
pub struct JsString(Vec<u16>, String);

impl JsString {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_utf16(units: impl Into<Vec<u16>>) -> Self {
        let units = units.into();
        let lossy = String::from_utf16_lossy(&units);
        Self(units, lossy)
    }

    pub fn as_utf16(&self) -> &[u16] {
        &self.0
    }

    pub fn into_utf16(self) -> Vec<u16> {
        self.0
    }

    pub fn to_utf8(&self) -> Result<String, std::string::FromUtf16Error> {
        String::from_utf16(&self.0)
    }

    pub fn to_utf8_lossy(&self) -> String {
        self.as_str().to_owned()
    }

    /// Produces a UTF-8 JSON source with isolated code units written as `\uXXXX`.
    pub fn to_json_source(&self) -> String {
        let mut output = String::with_capacity(self.len());
        let mut index = 0;
        while index < self.0.len() {
            let unit = self.0[index];
            if (0xd800..=0xdbff).contains(&unit)
                && let Some(&low) = self.0.get(index + 1)
                && (0xdc00..=0xdfff).contains(&low)
            {
                let scalar =
                    0x1_0000 + ((u32::from(unit) - 0xd800) << 10) + (u32::from(low) - 0xdc00);
                output.push(char::from_u32(scalar).expect("valid surrogate pair"));
                index += 2;
                continue;
            }
            if (0xd800..=0xdfff).contains(&unit) {
                push_unicode_escape(&mut output, unit);
            } else {
                output.push(char::from_u32(u32::from(unit)).expect("non-surrogate BMP unit"));
            }
            index += 1;
        }
        output
    }

    /// Returns a non-panicking UTF-8 view. Isolated UTF-16 surrogates are
    /// represented with U+FFFD; use [`Self::as_utf16`] or [`Self::to_wtf8`]
    /// when every code unit must remain observable.
    pub fn as_str(&self) -> &str {
        &self.1
    }

    pub fn try_as_str(&self) -> Option<&str> {
        self.is_well_formed().then(|| self.as_str())
    }

    pub fn is_well_formed(&self) -> bool {
        let mut index = 0;
        while index < self.0.len() {
            match self.0[index] {
                0xd800..=0xdbff
                    if self
                        .0
                        .get(index + 1)
                        .is_some_and(|unit| (0xdc00..=0xdfff).contains(unit)) =>
                {
                    index += 2;
                }
                0xd800..=0xdfff => return false,
                _ => index += 1,
            }
        }
        true
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn is_blank(&self) -> bool {
        self.0.iter().all(|unit| {
            matches!(
                unit,
                0x0009..=0x000d
                    | 0x0020
                    | 0x00a0
                    | 0x1680
                    | 0x2000..=0x200a
                    | 0x2028
                    | 0x2029
                    | 0x202f
                    | 0x205f
                    | 0x3000
                    | 0xfeff
            )
        })
    }

    pub fn slice(&self, start: usize, end: usize) -> Self {
        let start = start.min(self.len());
        let end = end.max(start).min(self.len());
        Self::from_utf16(self.0[start..end].to_vec())
    }

    pub fn join_refs<'a>(values: impl IntoIterator<Item = &'a Self>, separator: &str) -> Self {
        let mut output = Self::new();
        for (index, value) in values.into_iter().enumerate() {
            if index != 0 {
                output.push_utf8(separator);
            }
            output.push_str(value);
        }
        output
    }

    pub fn push_str(&mut self, value: impl Into<Self>) {
        let value = value.into();
        self.0.extend_from_slice(&value.0);
        self.1 = String::from_utf16_lossy(&self.0);
    }

    pub fn push_utf8(&mut self, value: &str) {
        self.0.extend(value.encode_utf16());
        self.1 = String::from_utf16_lossy(&self.0);
    }

    pub fn contains(&self, needle: impl Into<Self>) -> bool {
        let needle = needle.into();
        needle.is_empty()
            || self
                .0
                .windows(needle.len())
                .any(|window| window == needle.as_utf16())
    }

    pub fn starts_with(&self, prefix: impl Into<Self>) -> bool {
        let prefix = prefix.into();
        self.0.starts_with(&prefix.0)
    }

    pub fn json_quote(&self) -> String {
        let mut output = String::with_capacity(self.len() + 2);
        output.push('"');
        let mut index = 0;
        while index < self.0.len() {
            let unit = self.0[index];
            match unit {
                0x0008 => output.push_str("\\b"),
                0x0009 => output.push_str("\\t"),
                0x000a => output.push_str("\\n"),
                0x000c => output.push_str("\\f"),
                0x000d => output.push_str("\\r"),
                0x0000..=0x001f => push_unicode_escape(&mut output, unit),
                0x0022 => output.push_str("\\\""),
                0x005c => output.push_str("\\\\"),
                0xd800..=0xdbff => {
                    if let Some(&low) = self.0.get(index + 1)
                        && (0xdc00..=0xdfff).contains(&low)
                    {
                        let scalar = 0x1_0000
                            + ((u32::from(unit) - 0xd800) << 10)
                            + (u32::from(low) - 0xdc00);
                        output.push(char::from_u32(scalar).expect("valid surrogate pair"));
                        index += 1;
                    } else {
                        push_unicode_escape(&mut output, unit);
                    }
                }
                0xdc00..=0xdfff => push_unicode_escape(&mut output, unit),
                _ => output.push(char::from_u32(u32::from(unit)).expect("non-surrogate BMP unit")),
            }
            index += 1;
        }
        output.push('"');
        output
    }

    pub fn from_wtf8(bytes: &[u8]) -> Result<Self, &'static str> {
        let mut units = Vec::with_capacity(bytes.len());
        let mut index = 0;
        while index < bytes.len() {
            let first = bytes[index];
            let (scalar, width) = match first {
                0x00..=0x7f => (u32::from(first), 1),
                0xc2..=0xdf => (decode_wtf8(bytes, index, 2)?, 2),
                0xe0..=0xef => (decode_wtf8(bytes, index, 3)?, 3),
                0xf0..=0xf4 => (decode_wtf8(bytes, index, 4)?, 4),
                _ => return Err("invalid WTF-8 leading byte"),
            };
            if scalar <= 0xffff {
                units.push(u16::try_from(scalar).expect("BMP scalar"));
            } else if scalar <= 0x10ffff {
                let scalar = scalar - 0x1_0000;
                units.push(0xd800 + u16::try_from(scalar >> 10).expect("high ten bits"));
                units.push(0xdc00 + u16::try_from(scalar & 0x3ff).expect("low ten bits"));
            } else {
                return Err("WTF-8 scalar is out of range");
            }
            index += width;
        }
        Ok(Self::from_utf16(units))
    }

    pub fn to_wtf8(&self) -> Vec<u8> {
        let mut output = Vec::with_capacity(self.0.len());
        let mut index = 0;
        while index < self.0.len() {
            let unit = self.0[index];
            if (0xd800..=0xdbff).contains(&unit)
                && let Some(&low) = self.0.get(index + 1)
                && (0xdc00..=0xdfff).contains(&low)
            {
                let scalar =
                    0x1_0000 + ((u32::from(unit) - 0xd800) << 10) + (u32::from(low) - 0xdc00);
                push_wtf8_scalar(&mut output, scalar);
                index += 2;
                continue;
            }
            push_wtf8_scalar(&mut output, u32::from(unit));
            index += 1;
        }
        output
    }
}

fn push_wtf8_scalar(output: &mut Vec<u8>, scalar: u32) {
    match scalar {
        0x0000..=0x007f => output.push(scalar as u8),
        0x0080..=0x07ff => {
            output.push(0xc0 | ((scalar >> 6) as u8));
            output.push(0x80 | ((scalar & 0x3f) as u8));
        }
        0x0800..=0xffff => {
            output.push(0xe0 | ((scalar >> 12) as u8));
            output.push(0x80 | (((scalar >> 6) & 0x3f) as u8));
            output.push(0x80 | ((scalar & 0x3f) as u8));
        }
        _ => {
            output.push(0xf0 | ((scalar >> 18) as u8));
            output.push(0x80 | (((scalar >> 12) & 0x3f) as u8));
            output.push(0x80 | (((scalar >> 6) & 0x3f) as u8));
            output.push(0x80 | ((scalar & 0x3f) as u8));
        }
    }
}

fn decode_wtf8(bytes: &[u8], index: usize, width: usize) -> Result<u32, &'static str> {
    let sequence = bytes
        .get(index..index + width)
        .ok_or("truncated WTF-8 sequence")?;
    if sequence[1..]
        .iter()
        .any(|byte| !matches!(byte, 0x80..=0xbf))
    {
        return Err("invalid WTF-8 continuation byte");
    }
    let scalar = match width {
        2 => (u32::from(sequence[0] & 0x1f) << 6) | u32::from(sequence[1] & 0x3f),
        3 => {
            (u32::from(sequence[0] & 0x0f) << 12)
                | (u32::from(sequence[1] & 0x3f) << 6)
                | u32::from(sequence[2] & 0x3f)
        }
        4 => {
            (u32::from(sequence[0] & 0x07) << 18)
                | (u32::from(sequence[1] & 0x3f) << 12)
                | (u32::from(sequence[2] & 0x3f) << 6)
                | u32::from(sequence[3] & 0x3f)
        }
        _ => return Err("invalid WTF-8 width"),
    };
    let minimum = match width {
        2 => 0x80,
        3 => 0x800,
        4 => 0x1_0000,
        _ => unreachable!(),
    };
    if scalar < minimum || scalar > 0x10ffff {
        return Err("overlong or out-of-range WTF-8 sequence");
    }
    Ok(scalar)
}

fn push_unicode_escape(output: &mut String, unit: u16) {
    use std::fmt::Write as _;
    write!(output, "\\u{unit:04x}").expect("writing to String cannot fail");
}

impl From<&str> for JsString {
    fn from(value: &str) -> Self {
        Self(value.encode_utf16().collect(), value.to_owned())
    }
}

impl From<String> for JsString {
    fn from(value: String) -> Self {
        Self::from(value.as_str())
    }
}

impl From<&String> for JsString {
    fn from(value: &String) -> Self {
        Self::from(value.as_str())
    }
}

impl From<&JsString> for JsString {
    fn from(value: &JsString) -> Self {
        value.clone()
    }
}

impl From<Vec<u16>> for JsString {
    fn from(value: Vec<u16>) -> Self {
        Self::from_utf16(value)
    }
}

impl Clone for JsString {
    fn clone(&self) -> Self {
        Self(self.0.clone(), self.1.clone())
    }
}

impl Default for JsString {
    fn default() -> Self {
        Self::from_utf16(Vec::new())
    }
}

impl PartialEq for JsString {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Eq for JsString {}

impl PartialOrd for JsString {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for JsString {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.cmp(&other.0)
    }
}

impl Hash for JsString {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

impl Deref for JsString {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.as_str()
    }
}

impl AddAssign<&JsString> for JsString {
    fn add_assign(&mut self, rhs: &JsString) {
        self.push_str(rhs);
    }
}

impl PartialEq<str> for JsString {
    fn eq(&self, other: &str) -> bool {
        self.0.iter().copied().eq(other.encode_utf16())
    }
}

impl PartialEq<&str> for JsString {
    fn eq(&self, other: &&str) -> bool {
        self == *other
    }
}

impl PartialEq<String> for JsString {
    fn eq(&self, other: &String) -> bool {
        self == other.as_str()
    }
}

impl PartialEq<JsString> for String {
    fn eq(&self, other: &JsString) -> bool {
        other == self
    }
}

impl AsRef<str> for JsString {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl std::borrow::Borrow<[u16]> for JsString {
    fn borrow(&self) -> &[u16] {
        self.as_utf16()
    }
}

impl fmt::Debug for JsString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.json_quote())
    }
}

impl fmt::Display for JsString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl Serialize for JsString {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let raw = RawValue::from_string(self.json_quote()).map_err(S::Error::custom)?;
        raw.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for JsString {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct JsStringVisitor;

        impl<'de> Visitor<'de> for JsStringVisitor {
            type Value = JsString;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an ECMAScript UTF-16 string")
            }

            fn visit_bytes<E>(self, value: &[u8]) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                JsString::from_wtf8(value).map_err(E::custom)
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(JsString::from(value))
            }

            fn visit_borrowed_str<E>(self, value: &'de str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                self.visit_str(value)
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(JsString::from(value))
            }

            fn visit_borrowed_bytes<E>(self, value: &'de [u8]) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                self.visit_bytes(value)
            }

            fn visit_byte_buf<E>(self, value: Vec<u8>) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                self.visit_bytes(&value)
            }
        }

        deserializer.deserialize_bytes(JsStringVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::JsString;
    use std::collections::{HashMap, HashSet};

    #[test]
    fn serde_json_round_trips_lone_surrogates() {
        let value: JsString = serde_json::from_str(r#""a\ud83db\ude00c""#).expect("parse");
        assert_eq!(value.as_utf16(), &[0x61, 0xd83d, 0x62, 0xde00, 0x63]);
        assert_eq!(
            serde_json::to_string(&value).expect("serialize"),
            r#""a\ud83db\ude00c""#
        );
    }

    #[test]
    fn slices_exact_utf16_units() {
        let value = JsString::from("abc😀def");
        assert_eq!(value.slice(0, 4).as_utf16(), &[0x61, 0x62, 0x63, 0xd83d]);
        assert_eq!(value.slice(4, 8).as_utf16(), &[0xde00, 0x64, 0x65, 0x66]);
    }

    /// Pins the lossless string boundary required by pi `types.ts:350-467`:
    /// ordinary text conversions do not JSON-escape, and isolated surrogates
    /// never make the public convenience view panic.
    #[test]
    fn ordinary_display_is_unescaped_and_invalid_utf16_views_do_not_panic() {
        let ordinary = JsString::from("line 1\n\"line 2\"");
        assert_eq!(ordinary.to_string(), "line 1\n\"line 2\"");
        assert_eq!(ordinary.try_as_str(), Some("line 1\n\"line 2\""));

        let isolated = JsString::from_utf16(vec![0xd83d]);
        assert_eq!(isolated.as_str(), "�");
        assert_eq!(isolated.try_as_str(), None);
        assert_eq!(isolated.to_wtf8(), vec![0xed, 0xa0, 0xbd]);
        assert_eq!(
            JsString::from_wtf8(&isolated.to_wtf8()).expect("WTF-8"),
            isolated
        );
    }

    #[test]
    fn borrowed_collections_lookup_by_exact_utf16_units() {
        let high = JsString::from_utf16(vec![0xd83d]);
        let low = JsString::from_utf16(vec![0xde00]);
        assert_eq!(high.as_str(), low.as_str());

        let map = HashMap::from([(high.clone(), "high"), (low.clone(), "low")]);
        assert_eq!(map.get([0xd83d].as_slice()), Some(&"high"));
        assert_eq!(map.get([0xde00].as_slice()), Some(&"low"));

        let set = HashSet::from([high, low]);
        assert!(set.contains([0xd83d].as_slice()));
        assert!(set.contains([0xde00].as_slice()));
    }
}
