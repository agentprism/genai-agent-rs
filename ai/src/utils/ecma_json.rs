//! ECMAScript JSON values, parsing, and `JSON.stringify` formatting.

use crate::utils::js_string::JsString;
use indexmap::IndexMap;
use serde::de::Error as _;
use serde::ser::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::value::RawValue;
use std::fmt;
use std::ops::{Deref, DerefMut, Index, IndexMut};

pub(crate) fn provider_string(value: &JsString) -> JsonValue {
    JsonValue::String(value.clone())
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct JsonObject(IndexMap<JsString, JsonValue>);

impl JsonObject {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(
        &mut self,
        key: impl Into<JsString>,
        value: impl Into<JsonValue>,
    ) -> Option<JsonValue> {
        self.0.insert(key.into(), value.into())
    }

    pub fn get(&self, key: &str) -> Option<&JsonValue> {
        self.0
            .iter()
            .find_map(|(candidate, value)| (candidate == key).then_some(value))
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut JsonValue> {
        self.0
            .iter_mut()
            .find_map(|(candidate, value)| (candidate == key).then_some(value))
    }

    pub fn remove(&mut self, key: &str) -> Option<JsonValue> {
        let index = self.0.get_index_of(&JsString::from(key))?;
        self.0.shift_remove_index(index).map(|(_, value)| value)
    }

    pub fn to_serde_json(&self) -> Result<serde_json::Value, StrictJsonError> {
        JsonValue::Object(self.clone()).to_serde_json()
    }

    /// Lowers values the way `JSON.stringify` does before handing an object to
    /// a provider SDK: non-finite numbers become `null` instead of rejecting or
    /// replacing the entire tool-input object.
    pub fn to_serde_json_with_stringify_semantics(
        &self,
    ) -> Result<serde_json::Value, StrictJsonError> {
        JsonValue::Object(self.clone()).to_serde_json_with_stringify_semantics()
    }

    pub(crate) fn to_provider_json(&self) -> JsonValue {
        JsonValue::Object(self.clone())
    }
}

impl Deref for JsonObject {
    type Target = IndexMap<JsString, JsonValue>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl fmt::Display for JsonObject {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&stringify_object(self))
    }
}

impl DerefMut for JsonObject {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl FromIterator<(JsString, JsonValue)> for JsonObject {
    fn from_iter<T: IntoIterator<Item = (JsString, JsonValue)>>(iter: T) -> Self {
        Self(iter.into_iter().collect())
    }
}

impl From<serde_json::Map<String, serde_json::Value>> for JsonObject {
    fn from(value: serde_json::Map<String, serde_json::Value>) -> Self {
        Self(
            value
                .into_iter()
                .map(|(key, value)| (key.into(), JsonValue::from(value)))
                .collect(),
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("tool-call arguments must be a JSON object")]
pub struct JsonObjectTypeError;

impl TryFrom<serde_json::Value> for JsonObject {
    type Error = JsonObjectTypeError;

    fn try_from(value: serde_json::Value) -> Result<Self, Self::Error> {
        match value {
            serde_json::Value::Object(value) => Ok(value.into()),
            _ => Err(JsonObjectTypeError),
        }
    }
}

impl TryFrom<JsonValue> for JsonObject {
    type Error = JsonObjectTypeError;

    fn try_from(value: JsonValue) -> Result<Self, Self::Error> {
        match value {
            JsonValue::Object(value) => Ok(value),
            _ => Err(JsonObjectTypeError),
        }
    }
}

impl IntoIterator for JsonObject {
    type Item = (JsString, JsonValue);
    type IntoIter = indexmap::map::IntoIter<JsString, JsonValue>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<'a> IntoIterator for &'a JsonObject {
    type Item = (&'a JsString, &'a JsonValue);
    type IntoIter = indexmap::map::Iter<'a, JsString, JsonValue>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl Index<&str> for JsonObject {
    type Output = JsonValue;

    fn index(&self, index: &str) -> &Self::Output {
        self.get(index).unwrap_or(&JsonValue::Null)
    }
}

impl IndexMut<&str> for JsonObject {
    fn index_mut(&mut self, index: &str) -> &mut Self::Output {
        self.get_mut(index)
            .unwrap_or_else(|| panic!("no entry found for key {index:?}"))
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub enum JsonValue {
    #[default]
    Null,
    Bool(bool),
    Number(f64),
    String(JsString),
    Array(Vec<JsonValue>),
    Object(JsonObject),
}

impl fmt::Display for JsonValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&stringify(self))
    }
}

impl JsonValue {
    pub fn as_object(&self) -> Option<&JsonObject> {
        match self {
            Self::Object(value) => Some(value),
            _ => None,
        }
    }

    pub fn as_object_mut(&mut self) -> Option<&mut JsonObject> {
        match self {
            Self::Object(value) => Some(value),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Self]> {
        match self {
            Self::Array(value) => Some(value),
            _ => None,
        }
    }

    pub fn as_array_mut(&mut self) -> Option<&mut Vec<Self>> {
        match self {
            Self::Array(value) => Some(value),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&JsString> {
        match self {
            Self::String(value) => Some(value),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Number(value) => Some(*value),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(value) => Some(*value),
            _ => None,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    pub fn get(&self, key: &str) -> Option<&Self> {
        self.as_object()?.get(key)
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut Self> {
        self.as_object_mut()?.get_mut(key)
    }

    pub fn to_serde_json(&self) -> Result<serde_json::Value, StrictJsonError> {
        match self {
            Self::Null => Ok(serde_json::Value::Null),
            Self::Bool(value) => Ok(serde_json::Value::Bool(*value)),
            Self::Number(value) if value.is_finite() => Ok(crate::types::js_f64_value(*value)),
            Self::Number(_) => Err(StrictJsonError::NonFiniteNumber),
            Self::String(value) => value
                .to_utf8()
                .map(serde_json::Value::String)
                .map_err(|_| StrictJsonError::UnpairedSurrogate),
            Self::Array(values) => values
                .iter()
                .map(Self::to_serde_json)
                .collect::<Result<Vec<_>, _>>()
                .map(serde_json::Value::Array),
            Self::Object(values) => {
                let mut object = serde_json::Map::new();
                for (key, value) in values {
                    let key = key
                        .to_utf8()
                        .map_err(|_| StrictJsonError::UnpairedSurrogate)?;
                    object.insert(key, value.to_serde_json()?);
                }
                Ok(serde_json::Value::Object(object))
            }
        }
    }

    pub fn to_serde_json_with_stringify_semantics(
        &self,
    ) -> Result<serde_json::Value, StrictJsonError> {
        match self {
            Self::Null => Ok(serde_json::Value::Null),
            Self::Bool(value) => Ok(serde_json::Value::Bool(*value)),
            Self::Number(value) if value.is_finite() => Ok(crate::types::js_f64_value(*value)),
            Self::Number(_) => Ok(serde_json::Value::Null),
            Self::String(value) => value
                .to_utf8()
                .map(serde_json::Value::String)
                .map_err(|_| StrictJsonError::UnpairedSurrogate),
            Self::Array(values) => values
                .iter()
                .map(Self::to_serde_json_with_stringify_semantics)
                .collect::<Result<Vec<_>, _>>()
                .map(serde_json::Value::Array),
            Self::Object(values) => {
                let mut object = serde_json::Map::new();
                for (key, value) in values {
                    let key = key
                        .to_utf8()
                        .map_err(|_| StrictJsonError::UnpairedSurrogate)?;
                    object.insert(key, value.to_serde_json_with_stringify_semantics()?);
                }
                Ok(serde_json::Value::Object(object))
            }
        }
    }
}

macro_rules! number_from {
    ($($kind:ty),+ $(,)?) => {
        $(
            impl From<$kind> for JsonValue {
                fn from(value: $kind) -> Self {
                    Self::Number(value as f64)
                }
            }
        )+
    };
}

number_from!(i8, i16, i32, i64, isize, u8, u16, u32, u64, usize, f32, f64);

impl From<bool> for JsonValue {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}

impl From<JsString> for JsonValue {
    fn from(value: JsString) -> Self {
        Self::String(value)
    }
}

impl From<String> for JsonValue {
    fn from(value: String) -> Self {
        Self::String(value.into())
    }
}

impl From<&str> for JsonValue {
    fn from(value: &str) -> Self {
        Self::String(value.into())
    }
}

impl From<Vec<JsonValue>> for JsonValue {
    fn from(value: Vec<JsonValue>) -> Self {
        Self::Array(value)
    }
}

impl From<JsonObject> for JsonValue {
    fn from(value: JsonObject) -> Self {
        Self::Object(value)
    }
}

impl From<serde_json::Value> for JsonValue {
    fn from(value: serde_json::Value) -> Self {
        match value {
            serde_json::Value::Null => Self::Null,
            serde_json::Value::Bool(value) => Self::Bool(value),
            serde_json::Value::Number(value) => Self::Number(
                value
                    .as_f64()
                    .expect("serde_json numbers always convert to binary64"),
            ),
            serde_json::Value::String(value) => Self::String(value.into()),
            serde_json::Value::Array(values) => {
                Self::Array(values.into_iter().map(Self::from).collect())
            }
            serde_json::Value::Object(values) => Self::Object(JsonObject(
                values
                    .into_iter()
                    .map(|(key, value)| (key.into(), Self::from(value)))
                    .collect(),
            )),
        }
    }
}

impl PartialEq<serde_json::Value> for JsonValue {
    fn eq(&self, other: &serde_json::Value) -> bool {
        self == &Self::from(other.clone())
    }
}

impl PartialEq<str> for JsonValue {
    fn eq(&self, other: &str) -> bool {
        self.as_str().is_some_and(|value| value == other)
    }
}

impl PartialEq<&str> for JsonValue {
    fn eq(&self, other: &&str) -> bool {
        self == *other
    }
}

impl PartialEq<bool> for JsonValue {
    fn eq(&self, other: &bool) -> bool {
        self.as_bool() == Some(*other)
    }
}

macro_rules! partial_eq_number {
    ($($kind:ty),+ $(,)?) => {
        $(
            impl PartialEq<$kind> for JsonValue {
                fn eq(&self, other: &$kind) -> bool {
                    self.as_f64() == Some(*other as f64)
                }
            }
        )+
    };
}

partial_eq_number!(i8, i16, i32, i64, isize, u8, u16, u32, u64, usize, f32, f64);

impl Index<&str> for JsonValue {
    type Output = JsonValue;

    fn index(&self, index: &str) -> &Self::Output {
        self.get(index).unwrap_or(&JsonValue::Null)
    }
}

impl Index<usize> for JsonValue {
    type Output = JsonValue;

    fn index(&self, index: usize) -> &Self::Output {
        self.as_array()
            .and_then(|values| values.get(index))
            .unwrap_or(&JsonValue::Null)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum StrictJsonError {
    #[error("non-finite number cannot cross a strict JSON boundary")]
    NonFiniteNumber,
    #[error("unpaired UTF-16 surrogate cannot cross a UTF-8 boundary")]
    UnpairedSurrogate,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    message: &'static str,
    position: usize,
}

impl fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} at position {}", self.message, self.position)
    }
}

impl std::error::Error for ParseError {}

pub fn parse(input: &str) -> Result<JsonValue, ParseError> {
    let mut parser = Parser {
        input,
        bytes: input.as_bytes(),
        index: 0,
    };
    parser.skip_whitespace();
    let value = parser.parse_value()?;
    parser.skip_whitespace();
    if parser.index != parser.bytes.len() {
        return Err(parser.error("trailing characters"));
    }
    Ok(value)
}

pub fn stringify(value: &JsonValue) -> String {
    let mut output = String::new();
    write_value(value, &mut output);
    output
}

pub fn stringify_object(value: &JsonObject) -> String {
    stringify(&JsonValue::Object(value.clone()))
}

pub(crate) trait ProviderJsonValue {
    fn to_ecma_json(&self) -> JsonValue;
}

impl ProviderJsonValue for JsonValue {
    fn to_ecma_json(&self) -> JsonValue {
        self.clone()
    }
}

impl ProviderJsonValue for serde_json::Value {
    fn to_ecma_json(&self) -> JsonValue {
        JsonValue::from(self.clone())
    }
}

pub(crate) fn stringify_provider_json<T: ProviderJsonValue + ?Sized>(value: &T) -> String {
    stringify(&value.to_ecma_json())
}

pub fn format_number(value: f64) -> String {
    if value.is_finite() {
        ryu_js::Buffer::new().format(value).to_owned()
    } else {
        "null".to_owned()
    }
}

fn write_value(value: &JsonValue, output: &mut String) {
    match value {
        JsonValue::Null => output.push_str("null"),
        JsonValue::Bool(value) => output.push_str(if *value { "true" } else { "false" }),
        JsonValue::Number(value) => output.push_str(&format_number(*value)),
        JsonValue::String(value) => output.push_str(&value.json_quote()),
        JsonValue::Array(values) => {
            output.push('[');
            for (index, value) in values.iter().enumerate() {
                if index != 0 {
                    output.push(',');
                }
                write_value(value, output);
            }
            output.push(']');
        }
        JsonValue::Object(values) => {
            output.push('{');
            let mut entries = values.iter().enumerate().collect::<Vec<_>>();
            entries.sort_by(|(left_index, (left, _)), (right_index, (right, _))| {
                match (array_index(left), array_index(right)) {
                    (Some(left), Some(right)) => left.cmp(&right),
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (None, None) => left_index.cmp(right_index),
                }
            });
            for (index, (_, (key, value))) in entries.into_iter().enumerate() {
                if index != 0 {
                    output.push(',');
                }
                output.push_str(&key.json_quote());
                output.push(':');
                write_value(value, output);
            }
            output.push('}');
        }
    }
}

fn array_index(key: &JsString) -> Option<u32> {
    let units = key.as_utf16();
    if units.is_empty() || (units.len() > 1 && units[0] == u16::from(b'0')) {
        return None;
    }
    let mut value = 0_u64;
    for unit in units {
        let digit = match *unit {
            0x30..=0x39 => u64::from(*unit - 0x30),
            _ => return None,
        };
        value = value.checked_mul(10)?.checked_add(digit)?;
        if value > u64::from(u32::MAX) - 1 {
            return None;
        }
    }
    u32::try_from(value).ok()
}

impl Serialize for JsonValue {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let raw = RawValue::from_string(stringify(self)).map_err(S::Error::custom)?;
        raw.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for JsonValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = Box::<RawValue>::deserialize(deserializer)?;
        parse(raw.get()).map_err(D::Error::custom)
    }
}

impl Serialize for JsonObject {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        JsonValue::Object(self.clone()).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for JsonObject {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        match JsonValue::deserialize(deserializer)? {
            JsonValue::Object(value) => Ok(value),
            _ => Err(D::Error::custom("expected a JSON object")),
        }
    }
}

struct Parser<'a> {
    input: &'a str,
    bytes: &'a [u8],
    index: usize,
}

impl Parser<'_> {
    fn parse_value(&mut self) -> Result<JsonValue, ParseError> {
        self.skip_whitespace();
        match self.bytes.get(self.index) {
            Some(b'n') => self.keyword("null", JsonValue::Null),
            Some(b't') => self.keyword("true", JsonValue::Bool(true)),
            Some(b'f') => self.keyword("false", JsonValue::Bool(false)),
            Some(b'"') => self.parse_string().map(JsonValue::String),
            Some(b'[') => self.parse_array(),
            Some(b'{') => self.parse_object(),
            Some(b'-' | b'0'..=b'9') => self.parse_number().map(JsonValue::Number),
            Some(_) => Err(self.error("unexpected character")),
            None => Err(self.error("unexpected end of input")),
        }
    }

    fn keyword(
        &mut self,
        keyword: &'static str,
        value: JsonValue,
    ) -> Result<JsonValue, ParseError> {
        if self.input[self.index..].starts_with(keyword) {
            self.index += keyword.len();
            Ok(value)
        } else {
            Err(self.error("invalid keyword"))
        }
    }

    fn parse_string(&mut self) -> Result<JsString, ParseError> {
        self.index += 1;
        let mut units = Vec::new();
        let mut segment_start = self.index;
        while let Some(&byte) = self.bytes.get(self.index) {
            match byte {
                b'"' => {
                    self.push_utf8_segment(segment_start, self.index, &mut units)?;
                    self.index += 1;
                    return Ok(JsString::from_utf16(units));
                }
                b'\\' => {
                    self.push_utf8_segment(segment_start, self.index, &mut units)?;
                    self.index += 1;
                    let escape = *self
                        .bytes
                        .get(self.index)
                        .ok_or_else(|| self.error("unterminated escape"))?;
                    self.index += 1;
                    match escape {
                        b'"' => units.push(0x22),
                        b'\\' => units.push(0x5c),
                        b'/' => units.push(0x2f),
                        b'b' => units.push(0x08),
                        b'f' => units.push(0x0c),
                        b'n' => units.push(0x0a),
                        b'r' => units.push(0x0d),
                        b't' => units.push(0x09),
                        b'u' => units.push(self.parse_hex_unit()?),
                        _ => return Err(self.error("invalid escape")),
                    }
                    segment_start = self.index;
                }
                0x00..=0x1f => return Err(self.error("control character in string")),
                _ => self.index += 1,
            }
        }
        Err(self.error("unterminated string"))
    }

    fn push_utf8_segment(
        &self,
        start: usize,
        end: usize,
        units: &mut Vec<u16>,
    ) -> Result<(), ParseError> {
        let segment = self
            .input
            .get(start..end)
            .ok_or_else(|| self.error("invalid UTF-8 string boundary"))?;
        units.extend(segment.encode_utf16());
        Ok(())
    }

    fn parse_hex_unit(&mut self) -> Result<u16, ParseError> {
        let digits = self
            .input
            .get(self.index..self.index + 4)
            .ok_or_else(|| self.error("incomplete unicode escape"))?;
        if !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(self.error("invalid unicode escape"));
        }
        self.index += 4;
        u16::from_str_radix(digits, 16).map_err(|_| self.error("invalid unicode escape"))
    }

    fn parse_array(&mut self) -> Result<JsonValue, ParseError> {
        self.index += 1;
        self.skip_whitespace();
        let mut values = Vec::new();
        if self.bytes.get(self.index) == Some(&b']') {
            self.index += 1;
            return Ok(JsonValue::Array(values));
        }
        loop {
            values.push(self.parse_value()?);
            self.skip_whitespace();
            match self.bytes.get(self.index) {
                Some(b',') => self.index += 1,
                Some(b']') => {
                    self.index += 1;
                    return Ok(JsonValue::Array(values));
                }
                _ => return Err(self.error("expected comma or closing bracket")),
            }
        }
    }

    fn parse_object(&mut self) -> Result<JsonValue, ParseError> {
        self.index += 1;
        self.skip_whitespace();
        let mut values = JsonObject::new();
        if self.bytes.get(self.index) == Some(&b'}') {
            self.index += 1;
            return Ok(JsonValue::Object(values));
        }
        loop {
            if self.bytes.get(self.index) != Some(&b'"') {
                return Err(self.error("expected object key"));
            }
            let key = self.parse_string()?;
            self.skip_whitespace();
            if self.bytes.get(self.index) != Some(&b':') {
                return Err(self.error("expected colon"));
            }
            self.index += 1;
            values.insert(key, self.parse_value()?);
            self.skip_whitespace();
            match self.bytes.get(self.index) {
                Some(b',') => {
                    self.index += 1;
                    self.skip_whitespace();
                }
                Some(b'}') => {
                    self.index += 1;
                    return Ok(JsonValue::Object(values));
                }
                _ => return Err(self.error("expected comma or closing brace")),
            }
        }
    }

    fn parse_number(&mut self) -> Result<f64, ParseError> {
        let start = self.index;
        if self.bytes.get(self.index) == Some(&b'-') {
            self.index += 1;
        }
        match self.bytes.get(self.index) {
            Some(b'0') => self.index += 1,
            Some(b'1'..=b'9') => {
                self.index += 1;
                while self.bytes.get(self.index).is_some_and(u8::is_ascii_digit) {
                    self.index += 1;
                }
            }
            _ => return Err(self.error("invalid number")),
        }
        if self.bytes.get(self.index) == Some(&b'.') {
            self.index += 1;
            let fraction_start = self.index;
            while self.bytes.get(self.index).is_some_and(u8::is_ascii_digit) {
                self.index += 1;
            }
            if self.index == fraction_start {
                return Err(self.error("invalid number fraction"));
            }
        }
        if matches!(self.bytes.get(self.index), Some(b'e' | b'E')) {
            self.index += 1;
            if matches!(self.bytes.get(self.index), Some(b'+' | b'-')) {
                self.index += 1;
            }
            let exponent_start = self.index;
            while self.bytes.get(self.index).is_some_and(u8::is_ascii_digit) {
                self.index += 1;
            }
            if self.index == exponent_start {
                return Err(self.error("invalid number exponent"));
            }
        }
        self.input[start..self.index]
            .parse::<f64>()
            .map_err(|_| self.error("invalid number"))
    }

    fn skip_whitespace(&mut self) {
        while self
            .bytes
            .get(self.index)
            .is_some_and(|byte| matches!(byte, b' ' | b'\n' | b'\r' | b'\t'))
        {
            self.index += 1;
        }
    }

    fn error(&self, message: &'static str) -> ParseError {
        ParseError {
            message,
            position: self.index,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stringify_matches_ecmascript_number_boundaries_and_key_order() {
        let value = parse(
            r#"{"b":1,"10":10,"2":2,"01":1,"negativeZero":-0,"small":0.000001,"tiny":0.0000001,"large":100000000000000000000,"huge":1e21,"rounded":9007199254740993}"#,
        )
        .expect("parse");
        assert_eq!(
            stringify(&value),
            r#"{"2":2,"10":10,"b":1,"01":1,"negativeZero":0,"small":0.000001,"tiny":1e-7,"large":100000000000000000000,"huge":1e+21,"rounded":9007199254740992}"#
        );
    }

    #[test]
    fn parse_uses_binary64_and_preserves_nonfinite_overflow() {
        let value = parse(r#"{"rounded":9007199254740993,"overflow":1e400}"#).expect("parse");
        assert_eq!(value["rounded"].as_f64(), Some(9_007_199_254_740_992.0));
        assert_eq!(value["overflow"].as_f64(), Some(f64::INFINITY));
        assert_eq!(
            stringify(&value),
            r#"{"rounded":9007199254740992,"overflow":null}"#
        );
    }

    #[test]
    fn json_value_round_trips_lone_surrogates_through_serde_json() {
        let value: JsonValue = serde_json::from_str(r#"{"x":"\ud83d"}"#).expect("parse");
        assert_eq!(value["x"].as_str().expect("string").as_utf16(), &[0xd83d]);
        assert_eq!(
            serde_json::to_string(&value).expect("serialize"),
            r#"{"x":"\ud83d"}"#
        );
    }

    /// Pins pi `types.ts:370-376`: tool arguments are object-valued, while
    /// every JSON-like value remains available below that object boundary.
    #[test]
    fn tool_argument_objects_reject_non_object_roots_without_panicking() {
        assert!(JsonObject::try_from(serde_json::json!(null)).is_err());
        assert!(JsonObject::try_from(serde_json::json!([])).is_err());
        assert!(JsonObject::try_from(JsonValue::Number(1.0)).is_err());
        assert!(JsonObject::try_from(serde_json::json!({"nested": [null, true]})).is_ok());
    }

    /// Pins provider replay of pi `types.ts:370-376`: `JSON.stringify` lowers
    /// nested NaN/infinities to null without dropping neighboring arguments.
    #[test]
    fn provider_object_lowering_preserves_dynamic_neighbors_and_nulls_nonfinite_numbers() {
        let mut nested = JsonObject::new();
        nested.insert("nan", f64::NAN);
        nested.insert("negative", -1e20_f64);
        nested.insert(
            "array",
            JsonValue::Array(vec![1.into(), f64::INFINITY.into()]),
        );

        assert_eq!(
            nested
                .to_serde_json_with_stringify_semantics()
                .expect("provider object"),
            serde_json::json!({
                "nan": null,
                "negative": -1e20_f64,
                "array": [1, null]
            })
        );
        assert_eq!(
            stringify_object(&nested),
            r#"{"nan":null,"negative":-100000000000000000000,"array":[1,null]}"#
        );
    }

    #[test]
    fn provider_formatter_preserves_surrogates_and_sorts_integer_keys() {
        let mut arguments = JsonObject::new();
        arguments.insert("10", JsonValue::String(JsString::from_utf16(vec![0xd83d])));
        arguments.insert(JsString::from_utf16(vec![0xde00]), "low");
        arguments.insert("2", f64::NAN);
        let mut payload = JsonObject::new();
        payload.insert("input", arguments.to_provider_json());
        assert_eq!(
            stringify_provider_json(&JsonValue::Object(payload)),
            "{\"input\":{\"2\":null,\"10\":\"\\ud83d\",\"\\ude00\":\"low\"}}"
        );
    }

    #[test]
    fn reserved_sentinel_shaped_objects_are_ordinary_user_values() {
        let value = parse(
            "{\"\\u0000agentprism.ecma-json.string\":\"\\\"\\\\ud83d\\\"\",\"\\u0000agentprism.ecma-json.object\":\"{}\"}",
        )
        .expect("parse");
        assert_eq!(
            stringify_provider_json(&value),
            "{\"\\u0000agentprism.ecma-json.string\":\"\\\"\\\\ud83d\\\"\",\"\\u0000agentprism.ecma-json.object\":\"{}\"}"
        );
    }
}
