use rust_genai_agent::parse_streaming_json;
use serde_json::json;

const SECURITY_TEST_DEPTH: usize = 10_000;

#[test]
fn excessive_incomplete_json_nesting_falls_back_without_recursing_unboundedly() {
    let cases = [
        "[".repeat(SECURITY_TEST_DEPTH),
        r#"{"key":"#.repeat(SECURITY_TEST_DEPTH),
        (0..SECURITY_TEST_DEPTH)
            .map(|index| if index % 2 == 0 { "[" } else { r#"{"key":"# })
            .collect::<String>(),
        // A mismatched closer must not reset the scan while the partial parser remains nested.
        "[0}".repeat(SECURITY_TEST_DEPTH),
    ];

    for raw in cases {
        assert_eq!(parse_streaming_json(&raw), json!({}));
    }
}

#[test]
fn nesting_scan_ignores_brackets_braces_and_escapes_inside_strings() {
    let payload = format!(
        r#"literal [{{]}} with an escaped quote \" and slash \\ then {}"#,
        "[{".repeat(SECURITY_TEST_DEPTH * 2)
    );
    let raw = serde_json::to_string(&json!({ "payload": payload }))
        .expect("serialize bracket-heavy string fixture");

    assert_eq!(
        parse_streaming_json(&raw),
        serde_json::from_str::<serde_json::Value>(&raw).expect("fixture is complete JSON")
    );
}
