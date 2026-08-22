//! Unicode-surrogate sanitization ⇐ pi `src/utils/sanitize-unicode.ts`.

use crate::types::JsString;

pub trait SurrogateText {
    fn sanitized_surrogates(&self) -> String;
}

impl SurrogateText for str {
    fn sanitized_surrogates(&self) -> String {
        self.to_owned()
    }
}

impl SurrogateText for String {
    fn sanitized_surrogates(&self) -> String {
        self.clone()
    }
}

impl SurrogateText for JsString {
    fn sanitized_surrogates(&self) -> String {
        sanitize_surrogates_utf16(self.as_utf16())
    }
}

pub fn sanitize_surrogates<T>(text: &T) -> String
where
    T: SurrogateText + ?Sized,
{
    text.sanitized_surrogates()
}

pub fn sanitize_surrogates_utf16(units: &[u16]) -> String {
    let mut sanitized = Vec::with_capacity(units.len());
    let mut index = 0;
    while index < units.len() {
        let unit = units[index];
        if (0xd800..=0xdbff).contains(&unit) {
            if let Some(&low) = units.get(index + 1)
                && (0xdc00..=0xdfff).contains(&low)
            {
                sanitized.push(unit);
                sanitized.push(low);
                index += 2;
                continue;
            }
        } else if !(0xdc00..=0xdfff).contains(&unit) {
            sanitized.push(unit);
        }
        index += 1;
    }
    String::from_utf16(&sanitized).expect("only paired UTF-16 surrogates remain")
}

#[cfg(test)]
mod tests {
    use super::{sanitize_surrogates, sanitize_surrogates_utf16};

    /// Hermetic equivalent of pi `test/unicode-surrogate.test.ts:80-117` valid-Unicode cases.
    #[test]
    fn preserves_valid_multilingual_and_non_bmp_text() {
        let text = "🙈 👍 ❤️ 🤔 🚀 äußersr こんにちは 你好 ∑∫∂√";
        assert_eq!(sanitize_surrogates(text), text);
        assert_eq!(
            sanitize_surrogates_utf16(&text.encode_utf16().collect::<Vec<_>>()),
            text
        );
    }

    /// pi's live test constructs this logical input at `test/unicode-surrogate.test.ts:255-278`.
    #[test]
    fn removes_unpaired_surrogates_from_utf16_input() {
        assert_eq!(
            sanitize_surrogates_utf16(&[u16::from(b'a'), 0xd83d, u16::from(b'b')]),
            "ab"
        );
        assert_eq!(
            sanitize_surrogates_utf16(&[u16::from(b'a'), 0xde48, u16::from(b'b')]),
            "ab"
        );
        assert_eq!(sanitize_surrogates_utf16(&[0xd83d, 0xde48]), "🙈");
        assert_eq!(
            sanitize_surrogates_utf16(&[0xd83d, 0xd83d, 0xde48, 0xde48]),
            "🙈"
        );
    }
}
