//! Full and partial JSON recovery ⇐ pi `src/utils/json-parse.ts` and pinned `partial-json` 0.1.7.

use serde::de::DeserializeOwned;
use serde_json::{Map, Number, Value};

pub fn repair_json(json: &str) -> String {
    let characters = json.chars().collect::<Vec<_>>();
    let mut repaired = String::with_capacity(json.len());
    let mut in_string = false;
    let mut index = 0;

    while index < characters.len() {
        let character = characters[index];
        if !in_string {
            repaired.push(character);
            if character == '"' {
                in_string = true;
            }
            index += 1;
            continue;
        }

        if character == '"' {
            repaired.push(character);
            in_string = false;
            index += 1;
            continue;
        }

        if character == '\\' {
            let Some(&next) = characters.get(index + 1) else {
                repaired.push_str("\\\\");
                index += 1;
                continue;
            };

            if next == 'u' {
                let digits = characters.get(index + 2..index + 6);
                if digits.is_some_and(|digits| {
                    digits.len() == 4 && digits.iter().all(|digit| digit.is_ascii_hexdigit())
                }) {
                    repaired.push_str("\\u");
                    for digit in digits.expect("checked above") {
                        repaired.push(*digit);
                    }
                    index += 6;
                    continue;
                }
            }

            if matches!(next, '"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't' | 'u') {
                repaired.push('\\');
                repaired.push(next);
                index += 2;
                continue;
            }

            repaired.push_str("\\\\");
            index += 1;
            continue;
        }

        match character {
            '\u{0008}' => repaired.push_str("\\b"),
            '\u{000c}' => repaired.push_str("\\f"),
            '\n' => repaired.push_str("\\n"),
            '\r' => repaired.push_str("\\r"),
            '\t' => repaired.push_str("\\t"),
            control if control <= '\u{001f}' => {
                repaired.push_str(&format!("\\u{:04x}", u32::from(control)));
            }
            value => repaired.push(value),
        }
        index += 1;
    }

    repaired
}

pub fn parse_json_with_repair<T>(json: &str) -> Result<T, serde_json::Error>
where
    T: DeserializeOwned,
{
    match serde_json::from_str(json) {
        Ok(value) => Ok(value),
        Err(original_error) => {
            let repaired = repair_json(json);
            if repaired == json {
                Err(original_error)
            } else {
                serde_json::from_str(&repaired)
            }
        }
    }
}

pub fn parse_streaming_json(partial_json: Option<&str>) -> Value {
    let Some(partial_json) = partial_json.filter(|json| !json.trim().is_empty()) else {
        return Value::Object(Map::new());
    };

    parse_json_with_repair(partial_json)
        .or_else(|_| PartialParser::parse(partial_json).map(coalesce_partial_null))
        .or_else(|_| PartialParser::parse(&repair_json(partial_json)).map(coalesce_partial_null))
        .unwrap_or_else(|_| Value::Object(Map::new()))
}

fn coalesce_partial_null(value: Value) -> Value {
    match value {
        Value::Null => Value::Object(Map::new()),
        value => value,
    }
}

#[derive(Debug, Clone, Copy)]
struct PartialParseError;

struct PartialParser<'a> {
    input: &'a str,
    bytes: &'a [u8],
    index: usize,
}

impl<'a> PartialParser<'a> {
    fn parse(input: &'a str) -> Result<Value, PartialParseError> {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Err(PartialParseError);
        }
        let mut parser = Self {
            input: trimmed,
            bytes: trimmed.as_bytes(),
            index: 0,
        };
        parser.parse_any()
    }

    fn parse_any(&mut self) -> Result<Value, PartialParseError> {
        self.skip_blank();
        let Some(&next) = self.bytes.get(self.index) else {
            return Err(PartialParseError);
        };
        match next {
            b'"' => self.parse_string().map(Value::String),
            b'{' => self.parse_object(),
            b'[' => self.parse_array(),
            _ if self.partial_keyword("null") => Ok(Value::Null),
            _ if self.partial_keyword("true") => Ok(Value::Bool(true)),
            _ if self.partial_keyword("false") => Ok(Value::Bool(false)),
            _ => self.parse_number().map(Value::Number),
        }
    }

    fn partial_keyword(&mut self, keyword: &str) -> bool {
        let remaining = &self.input[self.index..];
        if remaining.starts_with(keyword)
            || (remaining.len() < keyword.len() && keyword.starts_with(remaining))
        {
            self.index = self.index.saturating_add(keyword.len());
            true
        } else {
            false
        }
    }

    fn parse_string(&mut self) -> Result<String, PartialParseError> {
        let start = self.index;
        let mut escaped = false;
        self.index += 1;
        while self.index < self.bytes.len() {
            let byte = self.bytes[self.index];
            if byte == b'"' && !escaped {
                self.index += 1;
                return serde_json::from_str(&self.input[start..self.index])
                    .map_err(|_| PartialParseError);
            }
            escaped = byte == b'\\' && !escaped;
            if byte != b'\\' {
                escaped = false;
            }
            self.index += 1;
        }

        let mut end = self.index;
        if escaped {
            end = end.saturating_sub(1);
        }
        let candidate = format!("{}\"", &self.input[start..end]);
        if let Ok(value) = serde_json::from_str(&candidate) {
            return Ok(value);
        }

        let last_backslash = self.input[start..self.index]
            .rfind('\\')
            .map(|offset| start + offset)
            .ok_or(PartialParseError)?;
        serde_json::from_str(&format!("{}\"", &self.input[start..last_backslash]))
            .map_err(|_| PartialParseError)
    }

    fn parse_object(&mut self) -> Result<Value, PartialParseError> {
        self.index += 1;
        self.skip_blank();
        let mut object = Map::new();
        loop {
            self.skip_blank();
            match self.bytes.get(self.index) {
                Some(b'}') => {
                    self.index += 1;
                    return Ok(Value::Object(object));
                }
                None => return Ok(Value::Object(object)),
                Some(_) => {}
            }

            let key = match self.parse_string() {
                Ok(key) => key,
                Err(_) => return Ok(Value::Object(object)),
            };
            self.skip_blank();
            self.index = self.index.saturating_add(1);
            match self.parse_any() {
                Ok(value) => {
                    object.insert(key, value);
                }
                Err(_) => return Ok(Value::Object(object)),
            }
            self.skip_blank();
            if self.bytes.get(self.index) == Some(&b',') {
                self.index += 1;
            }
        }
    }

    fn parse_array(&mut self) -> Result<Value, PartialParseError> {
        self.index += 1;
        let mut array = Vec::new();
        loop {
            self.skip_blank();
            if self.bytes.get(self.index) == Some(&b']') {
                self.index += 1;
                return Ok(Value::Array(array));
            }
            if self.index >= self.bytes.len() {
                return Ok(Value::Array(array));
            }
            match self.parse_any() {
                Ok(value) => array.push(value),
                Err(_) => return Ok(Value::Array(array)),
            }
            self.skip_blank();
            if self.bytes.get(self.index) == Some(&b',') {
                self.index += 1;
            }
        }
    }

    fn parse_number(&mut self) -> Result<Number, PartialParseError> {
        let start = self.index;
        if self.bytes.get(self.index) == Some(&b'-') {
            self.index += 1;
        }
        while let Some(byte) = self.bytes.get(self.index) {
            if matches!(byte, b',' | b']' | b'}') {
                break;
            }
            self.index += 1;
        }
        let token = &self.input[start..self.index];
        if let Ok(number) = token.parse::<Number>() {
            return Ok(number);
        }
        if let Some(exponent) = token.rfind('e').or_else(|| token.rfind('E')) {
            return token[..exponent]
                .parse::<Number>()
                .map_err(|_| PartialParseError);
        }
        Err(PartialParseError)
    }

    fn skip_blank(&mut self) {
        while self
            .bytes
            .get(self.index)
            .is_some_and(|byte| matches!(byte, b' ' | b'\n' | b'\r' | b'\t'))
        {
            self.index += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Matrix evaluated through pi `parseStreamingJson` (`src/utils/json-parse.ts:104-123`).
    #[test]
    fn matches_pi_partial_parse_matrix() {
        for (input, expected) in [
            (None, json!({})),
            (Some(""), json!({})),
            (Some("   "), json!({})),
            (Some("{"), json!({})),
            (Some(r#"{"a":"#), json!({})),
            (Some(r#"{"a":1"#), json!({"a":1})),
            (Some(r#"{"a":1,"#), json!({"a":1})),
            (Some(r#"{"a":"hel"#), json!({"a":"hel"})),
            (Some(r#"{"a":[1,2,"#), json!({"a":[1,2]})),
            (Some(r#"[1,{"b":"x"#), json!([1,{"b":"x"}])),
            (Some("true"), json!(true)),
            (Some("tr"), json!(true)),
            (Some("n"), json!({})),
            (Some("nu"), json!({})),
            (Some("nul"), json!({})),
            (Some(" nu"), json!({})),
            (Some("null"), Value::Null),
            (Some("null "), Value::Null),
            (Some("12"), json!(12)),
            (Some("12e"), json!(12)),
            (Some(r#"{"a":true,"b":fa"#), json!({"a":true,"b":false})),
            (Some(r#"{"a":1} trailing"#), json!({"a":1})),
            (Some("garbage"), json!({})),
        ] {
            assert_eq!(parse_streaming_json(input), expected, "{input:?}");
        }
    }

    /// Cases evaluated through pi `repairJson` and `parseStreamingJson` (`src/utils/json-parse.ts:27-95`).
    #[test]
    fn repairs_invalid_escapes_and_raw_control_characters() {
        let cases = [
            (
                "{\"path\":\"A\\H\",\"text\":\"col1\tcol2\"}",
                json!({"path":"A\\H","text":"col1\tcol2"}),
            ),
            ("{\"x\":\"line\nnext\"}", json!({"x":"line\nnext"})),
            ("{\"x\":\"bad\\q\"}", json!({"x":"bad\\q"})),
            ("{\"x\":\"trail\\", json!({"x":"trail"})),
        ];
        for (input, expected) in cases {
            assert_eq!(parse_streaming_json(Some(input)), expected, "{input:?}");
        }
    }

    #[test]
    fn full_parse_rethrows_unrepairable_json() {
        assert!(parse_json_with_repair::<Value>("garbage").is_err());
        assert_eq!(
            parse_json_with_repair::<Value>("{\"x\":\"bad\\q\"}").expect("repaired"),
            json!({"x":"bad\\q"})
        );
    }
}
