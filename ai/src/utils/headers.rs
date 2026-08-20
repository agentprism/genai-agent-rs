//! Header conversion ⇐ pi `src/utils/headers.ts`.

use crate::types::ProviderHeaders;
use std::collections::BTreeMap;

pub fn headers_to_record(headers: &http::HeaderMap) -> BTreeMap<String, String> {
    headers
        .keys()
        .map(|key| {
            (
                key.as_str().to_owned(),
                headers
                    .get_all(key)
                    .iter()
                    .map(|value| {
                        value
                            .as_bytes()
                            .iter()
                            .copied()
                            .map(char::from)
                            .collect::<String>()
                    })
                    .collect::<Vec<_>>()
                    .join(", "),
            )
        })
        .collect()
}

pub fn provider_headers_to_record(
    headers: Option<&ProviderHeaders>,
) -> Option<BTreeMap<String, String>> {
    let result = headers?
        .iter()
        .filter_map(|(key, value)| value.as_ref().map(|value| (key.clone(), value.clone())))
        .collect::<BTreeMap<_, _>>();
    (!result.is_empty()).then_some(result)
}

#[cfg(test)]
mod tests {
    use super::{headers_to_record, provider_headers_to_record};
    use std::collections::BTreeMap;

    /// Derived from pi `src/utils/headers.ts:3-18`.
    #[test]
    fn converts_headers_and_suppresses_null_provider_values() {
        let mut entries = http::HeaderMap::new();
        entries.append("a", http::HeaderValue::from_static("1"));
        entries.append("a", http::HeaderValue::from_static("2"));
        entries.insert("b", http::HeaderValue::from_static("3"));
        assert_eq!(
            headers_to_record(&entries),
            BTreeMap::from([
                ("a".to_owned(), "1, 2".to_owned()),
                ("b".to_owned(), "3".to_owned()),
            ])
        );

        let provider = BTreeMap::from([
            ("keep".to_owned(), Some("yes".to_owned())),
            ("remove".to_owned(), None),
        ]);
        assert_eq!(
            provider_headers_to_record(Some(&provider)),
            Some(BTreeMap::from([("keep".to_owned(), "yes".to_owned())]))
        );
        assert_eq!(
            provider_headers_to_record(Some(&BTreeMap::from([("x".to_owned(), None)]))),
            None
        );
        assert_eq!(provider_headers_to_record(None), None);
    }
}
