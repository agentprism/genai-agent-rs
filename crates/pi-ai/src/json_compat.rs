//! Ordered JSON values and JavaScript `JSON.stringify`-compatible writing.
//!
//! Provider request bodies are the one parity boundary at which byte identity
//! with pinned Pi is required (Architecture v2 part 2 §10.8). These types keep
//! object insertion order, array order, ECMAScript UTF-16 strings, and binary64
//! numbers explicit instead of relying on an arbitrary map serializer.

use indexmap::IndexMap;
use serde::de::Error as _;
use serde::ser::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::value::RawValue;
use std::fmt;
use std::io;

/// An ECMAScript string represented as its exact UTF-16 code-unit sequence.
///
/// Rust `String` inputs are always well formed. The UTF-16 constructor exists
/// for FFI and compatibility inputs, including isolated surrogates. The wire
/// writer escapes an isolated surrogate exactly as modern `JSON.stringify`
/// does; provider text encoders should call [`Self::from_sanitized_utf16`] when
/// reproducing Pi's explicit `sanitizeSurrogates` boundary.
#[derive(Clone, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct OrderedJsonString(Vec<u16>);

impl OrderedJsonString {
    /// Creates an empty ECMAScript string.
    pub const fn new() -> Self {
        Self(Vec::new())
    }

    /// Preserves an exact sequence of ECMAScript UTF-16 code units.
    pub fn from_utf16(units: impl Into<Vec<u16>>) -> Self {
        Self(units.into())
    }

    /// Removes isolated surrogates with Pi's `sanitizeSurrogates` behavior.
    pub fn from_sanitized_utf16(units: &[u16]) -> Self {
        Self(
            crate::sanitize_utf16_surrogates(units)
                .encode_utf16()
                .collect(),
        )
    }

    /// Returns the exact UTF-16 code units.
    pub fn as_utf16(&self) -> &[u16] {
        &self.0
    }

    /// Returns UTF-8 when the stored sequence contains no isolated surrogate.
    pub fn to_utf8(&self) -> Result<String, std::string::FromUtf16Error> {
        String::from_utf16(&self.0)
    }

    fn equals_str(&self, candidate: &str) -> bool {
        self.0.iter().copied().eq(candidate.encode_utf16())
    }
}

impl fmt::Debug for OrderedJsonString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&quote_ecmascript_string(self))
    }
}

impl From<&str> for OrderedJsonString {
    fn from(value: &str) -> Self {
        Self(value.encode_utf16().collect())
    }
}

impl From<String> for OrderedJsonString {
    fn from(value: String) -> Self {
        Self::from(value.as_str())
    }
}

impl From<&String> for OrderedJsonString {
    fn from(value: &String) -> Self {
        Self::from(value.as_str())
    }
}

impl Serialize for OrderedJsonString {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let raw = RawValue::from_string(quote_ecmascript_string(self)).map_err(S::Error::custom)?;
        raw.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for OrderedJsonString {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = Box::<RawValue>::deserialize(deserializer)?;
        match parse_ordered_json(raw.get()).map_err(D::Error::custom)? {
            OrderedJsonValue::String(value) => Ok(value),
            _ => Err(D::Error::custom("expected a JSON string")),
        }
    }
}

/// A JSON object that retains JavaScript property insertion order.
///
/// At write time, ECMAScript array-index keys are moved before other keys and
/// sorted numerically, exactly like `Object.keys`/`JSON.stringify`. Updating an
/// existing key changes its value without moving its insertion position.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OrderedJsonObject(IndexMap<OrderedJsonString, OrderedJsonValue>);

impl OrderedJsonObject {
    /// Creates an empty object.
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts or replaces a property.
    pub fn insert(
        &mut self,
        key: impl Into<OrderedJsonString>,
        value: impl Into<OrderedJsonValue>,
    ) -> Option<OrderedJsonValue> {
        self.0.insert(key.into(), value.into())
    }

    /// Gets a property by a well-formed Rust string key.
    pub fn get(&self, key: &str) -> Option<&OrderedJsonValue> {
        self.0
            .iter()
            .find_map(|(candidate, value)| candidate.equals_str(key).then_some(value))
    }

    /// Gets a mutable property by a well-formed Rust string key.
    pub fn get_mut(&mut self, key: &str) -> Option<&mut OrderedJsonValue> {
        self.0
            .iter_mut()
            .find_map(|(candidate, value)| candidate.equals_str(key).then_some(value))
    }

    /// Removes a property without disturbing the order of remaining members.
    pub fn remove(&mut self, key: &str) -> Option<OrderedJsonValue> {
        let index = self
            .0
            .iter()
            .position(|(candidate, _)| candidate.equals_str(key))?;
        self.0.shift_remove_index(index).map(|(_, value)| value)
    }

    /// Returns the number of stored properties, including absent properties
    /// that will be omitted by the writer.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Returns whether the object stores no properties.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Iterates in insertion order before ECMAScript key reordering.
    pub fn iter(&self) -> indexmap::map::Iter<'_, OrderedJsonString, OrderedJsonValue> {
        self.0.iter()
    }
}

impl<K, V> FromIterator<(K, V)> for OrderedJsonObject
where
    K: Into<OrderedJsonString>,
    V: Into<OrderedJsonValue>,
{
    fn from_iter<T: IntoIterator<Item = (K, V)>>(iter: T) -> Self {
        let mut object = Self::new();
        for (key, value) in iter {
            object.insert(key, value);
        }
        object
    }
}

impl IntoIterator for OrderedJsonObject {
    type Item = (OrderedJsonString, OrderedJsonValue);
    type IntoIter = indexmap::map::IntoIter<OrderedJsonString, OrderedJsonValue>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<'a> IntoIterator for &'a OrderedJsonObject {
    type Item = (&'a OrderedJsonString, &'a OrderedJsonValue);
    type IntoIter = indexmap::map::Iter<'a, OrderedJsonString, OrderedJsonValue>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl Serialize for OrderedJsonObject {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        OrderedJsonValue::Object(self.clone()).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for OrderedJsonObject {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = Box::<RawValue>::deserialize(deserializer)?;
        match parse_ordered_json(raw.get()).map_err(D::Error::custom)? {
            OrderedJsonValue::Object(value) => Ok(value),
            _ => Err(D::Error::custom("expected a JSON object")),
        }
    }
}

/// A JSON array with stable element order.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OrderedJsonArray(Vec<OrderedJsonValue>);

impl OrderedJsonArray {
    /// Creates an empty array.
    pub const fn new() -> Self {
        Self(Vec::new())
    }

    /// Appends an element.
    pub fn push(&mut self, value: impl Into<OrderedJsonValue>) {
        self.0.push(value.into());
    }

    /// Returns the number of elements.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Returns whether the array has no elements.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Returns the ordered elements.
    pub fn as_slice(&self) -> &[OrderedJsonValue] {
        &self.0
    }

    /// Iterates in array order.
    pub fn iter(&self) -> std::slice::Iter<'_, OrderedJsonValue> {
        self.0.iter()
    }
}

impl<V> FromIterator<V> for OrderedJsonArray
where
    V: Into<OrderedJsonValue>,
{
    fn from_iter<T: IntoIterator<Item = V>>(iter: T) -> Self {
        Self(iter.into_iter().map(Into::into).collect())
    }
}

impl IntoIterator for OrderedJsonArray {
    type Item = OrderedJsonValue;
    type IntoIter = std::vec::IntoIter<OrderedJsonValue>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<'a> IntoIterator for &'a OrderedJsonArray {
    type Item = &'a OrderedJsonValue;
    type IntoIter = std::slice::Iter<'a, OrderedJsonValue>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl Serialize for OrderedJsonArray {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        OrderedJsonValue::Array(self.clone()).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for OrderedJsonArray {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = Box::<RawValue>::deserialize(deserializer)?;
        match parse_ordered_json(raw.get()).map_err(D::Error::custom)? {
            OrderedJsonValue::Array(value) => Ok(value),
            _ => Err(D::Error::custom("expected a JSON array")),
        }
    }
}

/// One JavaScript-compatible ordered JSON value.
///
/// `Number` deliberately stores binary64 because Pi values are ECMAScript
/// `Number`s. `Absent` models an absent/`undefined` field: object members are
/// omitted, while array slots become `null`, matching `JSON.stringify`.
#[derive(Clone, Debug, Default)]
pub enum OrderedJsonValue {
    /// An absent object property or JavaScript `undefined` array element.
    Absent,
    /// JSON null.
    #[default]
    Null,
    /// JSON boolean.
    Bool(bool),
    /// ECMAScript binary64 number, including non-finite values.
    Number(f64),
    /// ECMAScript UTF-16 string.
    String(OrderedJsonString),
    /// Stable ordered array.
    Array(OrderedJsonArray),
    /// Insertion-ordered object.
    Object(OrderedJsonObject),
}

impl PartialEq for OrderedJsonValue {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Absent, Self::Absent) | (Self::Null, Self::Null) => true,
            (Self::Bool(left), Self::Bool(right)) => left == right,
            (Self::Number(left), Self::Number(right)) => left.to_bits() == right.to_bits(),
            (Self::String(left), Self::String(right)) => left == right,
            (Self::Array(left), Self::Array(right)) => left == right,
            (Self::Object(left), Self::Object(right)) => left == right,
            _ => false,
        }
    }
}

impl Eq for OrderedJsonValue {}

impl OrderedJsonValue {
    /// Returns this value as an object.
    pub const fn as_object(&self) -> Option<&OrderedJsonObject> {
        match self {
            Self::Object(value) => Some(value),
            _ => None,
        }
    }

    /// Returns this value as an array.
    pub const fn as_array(&self) -> Option<&OrderedJsonArray> {
        match self {
            Self::Array(value) => Some(value),
            _ => None,
        }
    }

    /// Returns this value as a finite or non-finite binary64 number.
    pub const fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Number(value) => Some(*value),
            _ => None,
        }
    }

    /// Returns this value as an exact ECMAScript string.
    pub const fn as_string(&self) -> Option<&OrderedJsonString> {
        match self {
            Self::String(value) => Some(value),
            _ => None,
        }
    }
}

impl Serialize for OrderedJsonValue {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let source = OrderedJsonWriter::stringify(self).map_err(S::Error::custom)?;
        let raw = RawValue::from_string(source).map_err(S::Error::custom)?;
        raw.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for OrderedJsonValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = Box::<RawValue>::deserialize(deserializer)?;
        parse_ordered_json(raw.get()).map_err(D::Error::custom)
    }
}

impl From<bool> for OrderedJsonValue {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}

macro_rules! number_from {
    ($($kind:ty),+ $(,)?) => {
        $(
            impl From<$kind> for OrderedJsonValue {
                fn from(value: $kind) -> Self {
                    Self::Number(value as f64)
                }
            }
        )+
    };
}

number_from!(i8, i16, i32, i64, isize, u8, u16, u32, u64, usize, f32, f64);

impl From<OrderedJsonString> for OrderedJsonValue {
    fn from(value: OrderedJsonString) -> Self {
        Self::String(value)
    }
}

impl From<String> for OrderedJsonValue {
    fn from(value: String) -> Self {
        Self::String(value.into())
    }
}

impl From<&str> for OrderedJsonValue {
    fn from(value: &str) -> Self {
        Self::String(value.into())
    }
}

impl From<OrderedJsonArray> for OrderedJsonValue {
    fn from(value: OrderedJsonArray) -> Self {
        Self::Array(value)
    }
}

impl From<Vec<OrderedJsonValue>> for OrderedJsonValue {
    fn from(value: Vec<OrderedJsonValue>) -> Self {
        Self::Array(value.into_iter().collect())
    }
}

impl From<OrderedJsonObject> for OrderedJsonValue {
    fn from(value: OrderedJsonObject) -> Self {
        Self::Object(value)
    }
}

impl From<serde_json::Value> for OrderedJsonValue {
    fn from(value: serde_json::Value) -> Self {
        match value {
            serde_json::Value::Null => Self::Null,
            serde_json::Value::Bool(value) => Self::Bool(value),
            serde_json::Value::Number(value) => Self::Number(value.as_f64().unwrap_or(f64::NAN)),
            serde_json::Value::String(value) => Self::String(value.into()),
            serde_json::Value::Array(values) => {
                Self::Array(values.into_iter().map(Self::from).collect())
            }
            serde_json::Value::Object(values) => Self::Object(
                values
                    .into_iter()
                    .map(|(key, value)| (key, Self::from(value)))
                    .collect(),
            ),
        }
    }
}

impl PartialEq<serde_json::Value> for OrderedJsonValue {
    fn eq(&self, other: &serde_json::Value) -> bool {
        self == &Self::from(other.clone())
    }
}

/// Failure from the ordered JSON wire writer.
#[derive(Debug)]
pub enum OrderedJsonWriteError {
    /// `JSON.stringify(undefined)` has no JSON string result.
    AbsentRoot,
    /// The destination rejected output bytes.
    Io(io::Error),
}

impl fmt::Display for OrderedJsonWriteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AbsentRoot => formatter.write_str("an absent root has no JSON representation"),
            Self::Io(error) => write!(formatter, "could not write JSON: {error}"),
        }
    }
}

impl std::error::Error for OrderedJsonWriteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::AbsentRoot => None,
            Self::Io(error) => Some(error),
        }
    }
}

impl From<io::Error> for OrderedJsonWriteError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

/// Stateless writer for byte-exact `JSON.stringify` output.
#[derive(Clone, Copy, Debug, Default)]
pub struct OrderedJsonWriter;

impl OrderedJsonWriter {
    /// Serializes to a compact UTF-8 string.
    pub fn stringify(value: &OrderedJsonValue) -> Result<String, OrderedJsonWriteError> {
        if matches!(value, OrderedJsonValue::Absent) {
            return Err(OrderedJsonWriteError::AbsentRoot);
        }
        let mut output = String::new();
        write_value(value, &mut output);
        Ok(output)
    }

    /// Serializes to compact UTF-8 bytes.
    pub fn to_vec(value: &OrderedJsonValue) -> Result<Vec<u8>, OrderedJsonWriteError> {
        Ok(Self::stringify(value)?.into_bytes())
    }

    /// Serializes to an arbitrary byte destination.
    pub fn write(
        value: &OrderedJsonValue,
        mut destination: impl io::Write,
    ) -> Result<(), OrderedJsonWriteError> {
        destination.write_all(&Self::to_vec(value)?)?;
        Ok(())
    }
}

/// Serializes any Serde value with the observable `JSON.stringify` rules used
/// by Pi. This compatibility helper keeps existing canonical-message and token
/// estimation callers on the same writer.
pub fn json_stringify_compatible<T>(value: &T) -> Result<String, serde_json::Error>
where
    T: Serialize + ?Sized,
{
    let ordered = OrderedJsonValue::from(serde_json::to_value(value)?);
    Ok(OrderedJsonWriter::stringify(&ordered)
        .expect("a Serde JSON root cannot be the explicit absent value"))
}

/// Parses JSON into ordered values using ECMAScript binary64 number and UTF-16
/// string semantics.
pub fn parse_ordered_json(
    input: impl AsRef<[u8]>,
) -> Result<OrderedJsonValue, OrderedJsonParseError> {
    let input = std::str::from_utf8(input.as_ref()).map_err(|_| OrderedJsonParseError {
        message: "input is not UTF-8",
        position: 0,
    })?;
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

/// Parse failure for [`parse_ordered_json`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OrderedJsonParseError {
    message: &'static str,
    position: usize,
}

impl OrderedJsonParseError {
    /// Byte offset at which parsing failed.
    pub const fn position(&self) -> usize {
        self.position
    }
}

impl fmt::Display for OrderedJsonParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} at byte {}", self.message, self.position)
    }
}

impl std::error::Error for OrderedJsonParseError {}

fn write_value(value: &OrderedJsonValue, output: &mut String) {
    match value {
        OrderedJsonValue::Absent | OrderedJsonValue::Null => output.push_str("null"),
        OrderedJsonValue::Bool(value) => output.push_str(if *value { "true" } else { "false" }),
        OrderedJsonValue::Number(value) => write_number(*value, output),
        OrderedJsonValue::String(value) => output.push_str(&quote_ecmascript_string(value)),
        OrderedJsonValue::Array(values) => {
            output.push('[');
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                write_value(value, output);
            }
            output.push(']');
        }
        OrderedJsonValue::Object(values) => write_object(values, output),
    }
}

fn write_number(value: f64, output: &mut String) {
    if value.is_finite() {
        output.push_str(ryu_js::Buffer::new().format(value));
    } else {
        output.push_str("null");
    }
}

fn write_object(values: &OrderedJsonObject, output: &mut String) {
    let mut entries = values.iter().enumerate().collect::<Vec<_>>();
    entries.sort_by(|(left_ordinal, (left, _)), (right_ordinal, (right, _))| {
        match (ecmascript_array_index(left), ecmascript_array_index(right)) {
            (Some(left), Some(right)) => left.cmp(&right),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => left_ordinal.cmp(right_ordinal),
        }
    });

    output.push('{');
    let mut first = true;
    for (_, (key, value)) in entries {
        if matches!(value, OrderedJsonValue::Absent) {
            continue;
        }
        if !first {
            output.push(',');
        }
        first = false;
        output.push_str(&quote_ecmascript_string(key));
        output.push(':');
        write_value(value, output);
    }
    output.push('}');
}

fn ecmascript_array_index(key: &OrderedJsonString) -> Option<u32> {
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
        if value >= u64::from(u32::MAX) {
            return None;
        }
    }
    u32::try_from(value).ok()
}

fn quote_ecmascript_string(value: &OrderedJsonString) -> String {
    let mut output = String::with_capacity(value.as_utf16().len() + 2);
    output.push('"');
    let mut index = 0;
    while let Some(&unit) = value.as_utf16().get(index) {
        match unit {
            0x0008 => output.push_str("\\b"),
            0x0009 => output.push_str("\\t"),
            0x000A => output.push_str("\\n"),
            0x000C => output.push_str("\\f"),
            0x000D => output.push_str("\\r"),
            0x0000..=0x001F => push_unicode_escape(&mut output, unit),
            0x0022 => output.push_str("\\\""),
            0x005C => output.push_str("\\\\"),
            0xD800..=0xDBFF => {
                if let Some(&low) = value.as_utf16().get(index + 1)
                    && (0xDC00..=0xDFFF).contains(&low)
                {
                    let scalar =
                        0x1_0000 + ((u32::from(unit) - 0xD800) << 10) + (u32::from(low) - 0xDC00);
                    output.push(char::from_u32(scalar).expect("valid surrogate pair"));
                    index += 1;
                } else {
                    push_unicode_escape(&mut output, unit);
                }
            }
            0xDC00..=0xDFFF => push_unicode_escape(&mut output, unit),
            _ => output.push(char::from_u32(u32::from(unit)).expect("non-surrogate BMP unit")),
        }
        index += 1;
    }
    output.push('"');
    output
}

fn push_unicode_escape(output: &mut String, unit: u16) {
    use std::fmt::Write as _;
    write!(output, "\\u{unit:04x}").expect("writing to String cannot fail");
}

struct Parser<'a> {
    input: &'a str,
    bytes: &'a [u8],
    index: usize,
}

impl Parser<'_> {
    fn parse_value(&mut self) -> Result<OrderedJsonValue, OrderedJsonParseError> {
        self.skip_whitespace();
        match self.bytes.get(self.index) {
            Some(b'n') => self.keyword("null", OrderedJsonValue::Null),
            Some(b't') => self.keyword("true", OrderedJsonValue::Bool(true)),
            Some(b'f') => self.keyword("false", OrderedJsonValue::Bool(false)),
            Some(b'"') => self.parse_string().map(OrderedJsonValue::String),
            Some(b'[') => self.parse_array(),
            Some(b'{') => self.parse_object(),
            Some(b'-' | b'0'..=b'9') => self.parse_number().map(OrderedJsonValue::Number),
            Some(_) => Err(self.error("unexpected character")),
            None => Err(self.error("unexpected end of input")),
        }
    }

    fn keyword(
        &mut self,
        keyword: &'static str,
        value: OrderedJsonValue,
    ) -> Result<OrderedJsonValue, OrderedJsonParseError> {
        if self.input[self.index..].starts_with(keyword) {
            self.index += keyword.len();
            Ok(value)
        } else {
            Err(self.error("invalid keyword"))
        }
    }

    fn parse_string(&mut self) -> Result<OrderedJsonString, OrderedJsonParseError> {
        self.index += 1;
        let mut units = Vec::new();
        let mut segment_start = self.index;
        while let Some(&byte) = self.bytes.get(self.index) {
            match byte {
                b'"' => {
                    self.push_utf8_segment(segment_start, self.index, &mut units)?;
                    self.index += 1;
                    return Ok(OrderedJsonString::from_utf16(units));
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
                        b'\\' => units.push(0x5C),
                        b'/' => units.push(0x2F),
                        b'b' => units.push(0x08),
                        b'f' => units.push(0x0C),
                        b'n' => units.push(0x0A),
                        b'r' => units.push(0x0D),
                        b't' => units.push(0x09),
                        b'u' => units.push(self.parse_hex_unit()?),
                        _ => return Err(self.error("invalid escape")),
                    }
                    segment_start = self.index;
                }
                0x00..=0x1F => return Err(self.error("control character in string")),
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
    ) -> Result<(), OrderedJsonParseError> {
        let segment = self
            .input
            .get(start..end)
            .ok_or_else(|| self.error("invalid UTF-8 string boundary"))?;
        units.extend(segment.encode_utf16());
        Ok(())
    }

    fn parse_hex_unit(&mut self) -> Result<u16, OrderedJsonParseError> {
        let digits = self
            .input
            .get(self.index..self.index.saturating_add(4))
            .ok_or_else(|| self.error("incomplete unicode escape"))?;
        if !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(self.error("invalid unicode escape"));
        }
        self.index += 4;
        u16::from_str_radix(digits, 16).map_err(|_| self.error("invalid unicode escape"))
    }

    fn parse_array(&mut self) -> Result<OrderedJsonValue, OrderedJsonParseError> {
        self.index += 1;
        self.skip_whitespace();
        let mut values = OrderedJsonArray::new();
        if self.bytes.get(self.index) == Some(&b']') {
            self.index += 1;
            return Ok(OrderedJsonValue::Array(values));
        }
        loop {
            values.push(self.parse_value()?);
            self.skip_whitespace();
            match self.bytes.get(self.index) {
                Some(b',') => {
                    self.index += 1;
                    self.skip_whitespace();
                }
                Some(b']') => {
                    self.index += 1;
                    return Ok(OrderedJsonValue::Array(values));
                }
                _ => return Err(self.error("expected comma or closing bracket")),
            }
        }
    }

    fn parse_object(&mut self) -> Result<OrderedJsonValue, OrderedJsonParseError> {
        self.index += 1;
        self.skip_whitespace();
        let mut values = OrderedJsonObject::new();
        if self.bytes.get(self.index) == Some(&b'}') {
            self.index += 1;
            return Ok(OrderedJsonValue::Object(values));
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
                    return Ok(OrderedJsonValue::Object(values));
                }
                _ => return Err(self.error("expected comma or closing brace")),
            }
        }
    }

    fn parse_number(&mut self) -> Result<f64, OrderedJsonParseError> {
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

    fn error(&self, message: &'static str) -> OrderedJsonParseError {
        OrderedJsonParseError {
            message,
            position: self.index,
        }
    }
}
