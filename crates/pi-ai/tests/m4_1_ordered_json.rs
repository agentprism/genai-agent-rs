use pi_ai::*;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

const PI_BASIS: &str = "architecture v2 part 2 §10.8 byte-comparison and turn-two contracts; JavaScript JSON.stringify; packages/ai/src/api/openai-completions.ts; packages/ai/src/api/anthropic-messages.ts; packages/ai/src/api/openai-responses.ts; packages/ai/src/api/openai-codex-responses.ts; packages/ai/src/api/simple-options.ts; packages/ai/src/utils/sanitize-unicode.ts";

fn pi_basis() {
    assert!(!PI_BASIS.is_empty());
}

fn wire(value: impl Into<OrderedJsonValue>) -> String {
    OrderedJsonWriter::stringify(&value.into()).expect("wire JSON")
}

#[test]
fn wire_json_object_insertion_order_matches_pi() {
    pi_basis();
    let mut object = OrderedJsonObject::new();
    object.insert("model", "fixture-model");
    object.insert("messages", OrderedJsonArray::new());
    object.insert("stream", true);
    object.insert("model", "replacement-model");

    assert_eq!(
        wire(object),
        r#"{"model":"replacement-model","messages":[],"stream":true}"#
    );
}

#[test]
fn wire_json_integer_like_keys_match_pi() {
    pi_basis();
    let mut object = OrderedJsonObject::new();
    object.insert("b", 1);
    object.insert("10", 10);
    object.insert("2", 2);
    object.insert("01", 1);
    object.insert("4294967294", 94);
    object.insert("4294967295", 95);
    object.insert("0", 0);
    object.insert("-0", -1);

    assert_eq!(
        wire(object),
        r#"{"0":0,"2":2,"10":10,"4294967294":94,"b":1,"01":1,"4294967295":95,"-0":-1}"#
    );
}

#[test]
fn wire_json_is_compact_and_arrays_are_stable() {
    pi_basis();
    let array = OrderedJsonArray::from_iter([
        OrderedJsonValue::from("first"),
        OrderedJsonValue::Object([("nested", true), ("second", false)].into_iter().collect()),
        OrderedJsonValue::from("last"),
    ]);

    assert_eq!(
        wire(array),
        r#"["first",{"nested":true,"second":false},"last"]"#
    );
}

#[test]
fn wire_json_absent_fields_match_pi() {
    pi_basis();
    let mut object = OrderedJsonObject::new();
    object.insert("present", 1);
    object.insert("absent", OrderedJsonValue::Absent);
    object.insert(
        "array",
        OrderedJsonArray::from_iter([
            OrderedJsonValue::Absent,
            OrderedJsonValue::Null,
            OrderedJsonValue::from(2),
        ]),
    );

    assert_eq!(wire(object), r#"{"present":1,"array":[null,null,2]}"#);
    assert!(matches!(
        OrderedJsonWriter::stringify(&OrderedJsonValue::Absent),
        Err(OrderedJsonWriteError::AbsentRoot)
    ));
}

#[derive(Serialize)]
struct SerdeAbsentFixture {
    present: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    absent: Option<u32>,
}

#[test]
fn wire_json_serde_absent_fields_are_omitted() {
    pi_basis();
    assert_eq!(
        json_stringify_compatible(&SerdeAbsentFixture {
            present: 7,
            absent: None,
        })
        .expect("serialize fixture"),
        r#"{"present":7}"#
    );
}

#[test]
fn wire_json_string_escaping_matches_pi() {
    pi_basis();
    let string = OrderedJsonString::from_utf16(vec![
        0x22, 0x5C, 0x2F, 0x08, 0x09, 0x0A, 0x0C, 0x0D, 0x00, 0x1F, 0x2028, 0x2029, 0xD83D, 0xDE00,
        0xD83D,
    ]);

    assert_eq!(
        wire(string),
        "\"\\\"\\\\/\\b\\t\\n\\f\\r\\u0000\\u001f\u{2028}\u{2029}😀\\ud83d\""
    );
}

#[test]
fn wire_json_numbers_match_pi() {
    pi_basis();
    let parsed = parse_ordered_json(
        r#"{"negativeZero":-0,"small":0.000001,"tiny":0.0000001,"large":100000000000000000000,"huge":1e21,"rounded":9007199254740993}"#,
    )
    .expect("parse fixture");

    assert_eq!(
        OrderedJsonWriter::stringify(&parsed).expect("write fixture"),
        r#"{"negativeZero":0,"small":0.000001,"tiny":1e-7,"large":100000000000000000000,"huge":1e+21,"rounded":9007199254740992}"#
    );
}

#[test]
fn wire_json_non_finite_numbers_become_null() {
    pi_basis();
    let mut object = OrderedJsonObject::new();
    object.insert("nan", f64::NAN);
    object.insert("positive", f64::INFINITY);
    object.insert("negative", f64::NEG_INFINITY);

    assert_eq!(
        wire(object),
        r#"{"nan":null,"positive":null,"negative":null}"#
    );
}

#[test]
fn wire_json_surrogate_sanitation_matches_pi() {
    pi_basis();
    let sanitized = OrderedJsonString::from_sanitized_utf16(&[
        u16::from(b'a'),
        0xD83D,
        u16::from(b'b'),
        0xD83D,
        0xDE00,
        0xDE00,
        u16::from(b'c'),
    ]);

    assert_eq!(wire(sanitized), r#""ab😀c""#);
}

#[test]
fn wire_json_parser_retains_order_and_ecmascript_strings() {
    pi_basis();
    let parsed =
        parse_ordered_json(r#"{"b":1,"2":2,"a":"\ud83d","b":4}"#).expect("parse ordered JSON");

    assert_eq!(
        OrderedJsonWriter::stringify(&parsed).expect("write ordered JSON"),
        r#"{"2":2,"b":4,"a":"\ud83d"}"#
    );
}

#[test]
fn wire_json_ordered_values_round_trip_through_serde() {
    pi_basis();
    let value =
        parse_ordered_json(r#"{"b":[1,true,null],"2":{"x":"😀"}}"#).expect("parse ordered JSON");
    let bytes = serde_json::to_vec(&value).expect("serialize ordered JSON");
    let restored: OrderedJsonValue = serde_json::from_slice(&bytes).expect("restore ordered JSON");

    assert_eq!(restored, value);
    assert_eq!(
        String::from_utf8(bytes).expect("UTF-8"),
        r#"{"2":{"x":"😀"},"b":[1,true,null]}"#
    );
}

const FIXTURE_CASES: &[&str] = &[
    "text-only",
    "system-developer-prompt",
    "images",
    "thinking-disabled",
    "reasoning-minimal",
    "reasoning-low",
    "reasoning-medium",
    "reasoning-high",
    "reasoning-xhigh",
    "reasoning-max",
    "signed-thinking-replay",
    "redacted-encrypted-reasoning-replay",
    "one-tool-call",
    "multiple-tool-calls",
    "tool-results",
    "tool-result-images",
    "orphan-result-repair",
    "cache-disabled",
    "cache-short",
    "cache-long",
    "sampling-defaults-and-overrides",
    "max-output-clamp",
    "strict-tool-schema",
    "provider-model-headers",
    "session-affinity",
    "api-specific-compat-flags",
    "cross-provider-handoff",
    "failed-turn-omission",
];

const FIXTURE_FILES: &[&str] = &[
    "canonical.json",
    "metadata.json",
    "request-turn-1.body.json",
    "request-turn-1.headers.json",
    "response-turn-1.sse",
    "request-turn-2.body.json",
    "request-turn-2.headers.json",
];

const SIMPLE_CASES: &[&str] = &[
    "thinking-disabled",
    "reasoning-minimal",
    "reasoning-low",
    "reasoning-medium",
    "reasoning-high",
    "reasoning-xhigh",
    "reasoning-max",
    "signed-thinking-replay",
    "redacted-encrypted-reasoning-replay",
    "sampling-defaults-and-overrides",
    "max-output-clamp",
];

const CREDENTIAL_CASES: &[&str] = &[
    "text-only",
    "one-tool-call",
    "tool-results",
    "signed-thinking-replay",
];

const UNSTABLE_HEADER_NAMES: &[&str] = &[
    "accept-encoding",
    "connection",
    "content-encoding",
    "content-length",
    "host",
    "user-agent",
];

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../providers/fixtures")
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn read_json(path: &Path) -> serde_json::Value {
    serde_json::from_slice(&fs::read(path).expect("read JSON artifact"))
        .expect("valid JSON artifact")
}

fn validate_header_artifact(path: &Path) {
    let artifact = read_json(path);
    assert_eq!(artifact["schemaVersion"], 1);
    let headers = artifact["headers"].as_object().expect("semantic headers");
    for (name, value) in headers {
        assert!(!UNSTABLE_HEADER_NAMES.contains(&name.as_str()));
        assert!(!name.starts_with("x-stainless-"));
        if matches!(
            name.as_str(),
            "authorization"
                | "proxy-authorization"
                | "x-api-key"
                | "api-key"
                | "cf-aig-authorization"
        ) {
            assert_eq!(value, "[REDACTED]");
        }
    }
    let omitted = artifact["omittedRuntimeHeaders"]
        .as_array()
        .expect("omitted runtime header names")
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect::<Vec<_>>();
    assert!(omitted.contains(&"host"));
    assert!(omitted.contains(&"user-agent"));
}

fn assert_turn_two_retains_simple_lowering(family: &str, case: &str, one: &[u8], two: &[u8]) {
    let turn_one: serde_json::Value = serde_json::from_slice(one).expect("turn-one body JSON");
    let turn_two: serde_json::Value = serde_json::from_slice(two).expect("turn-two body JSON");
    let max_field = match family {
        "anthropic-messages" | "openai-completions" => Some("max_tokens"),
        "openai-responses" => Some("max_output_tokens"),
        "openai-codex-responses" => None,
        _ => panic!("unknown fixture family: {family}"),
    };
    let Some(max_field) = max_field else {
        for field in ["temperature", "service_tier", "reasoning", "text"] {
            if turn_one.get(field).is_some() {
                assert_eq!(turn_one[field], turn_two[field], "{family}/{case} {field}");
            }
        }
        return;
    };
    let one_max = turn_one[max_field].as_u64().expect("turn-one max tokens");
    let two_max = turn_two[max_field].as_u64().expect("turn-two max tokens");
    assert!(one_max > 0 && two_max > 0);
    if case == "max-output-clamp" {
        assert!(one_max < 9_000 && two_max < 9_000);
    } else {
        assert_eq!(one_max, two_max, "{family}/{case} max-token lowering");
    }

    if family != "anthropic-messages" {
        for field in [
            "temperature",
            "top_p",
            "top_k",
            "seed",
            "reasoning_effort",
            "reasoning",
            "thinking",
            "enable_thinking",
            "chat_template_kwargs",
            "chat_template_args",
            "service_tier",
            "reasoning",
            "text",
        ] {
            if turn_one.get(field).is_some() {
                assert_eq!(turn_one[field], turn_two[field], "{family}/{case} {field}");
            }
        }
        if case.starts_with("reasoning-")
            || matches!(
                case,
                "signed-thinking-replay" | "redacted-encrypted-reasoning-replay"
            )
        {
            assert!(
                turn_two.get("reasoning_effort").is_some() || turn_two.get("reasoning").is_some()
            );
        }
        if case == "thinking-disabled" {
            if family == "openai-completions" {
                assert_eq!(turn_two["reasoning_effort"], "none");
            } else {
                assert_eq!(turn_two["reasoning"]["effort"], "none");
            }
        }
        if case == "sampling-defaults-and-overrides" {
            assert_eq!(turn_two["temperature"], 0.75);
            assert_eq!(turn_two["top_p"], 0.6);
            if family == "openai-completions" {
                assert_eq!(turn_two["top_k"], 40);
            }
            assert_eq!(turn_two["seed"], 7);
        }
    } else {
        for field in ["temperature", "thinking", "output_config"] {
            if turn_one.get(field).is_some() {
                assert_eq!(turn_one[field], turn_two[field], "{family}/{case} {field}");
            }
        }
        if case.starts_with("reasoning-")
            || matches!(
                case,
                "signed-thinking-replay" | "redacted-encrypted-reasoning-replay"
            )
        {
            assert!(turn_two.get("thinking").is_some());
        }
        if case == "thinking-disabled" {
            assert_eq!(turn_two["thinking"]["type"], "disabled");
        }
        if case == "sampling-defaults-and-overrides" {
            assert_eq!(turn_two["temperature"], 0.0);
        }
    }
}

fn validate_fixture_family(family: &str) {
    pi_basis();
    let family_root = fixture_root().join(family);
    let mut actual_cases = fs::read_dir(&family_root)
        .expect("read fixture family")
        .filter_map(|entry| {
            let entry = entry.expect("read fixture entry");
            entry
                .file_type()
                .expect("read fixture type")
                .is_dir()
                .then(|| entry.file_name().to_string_lossy().into_owned())
        })
        .collect::<Vec<_>>();
    actual_cases.sort();
    let mut expected_cases = FIXTURE_CASES
        .iter()
        .map(|case| (*case).to_owned())
        .collect::<Vec<_>>();
    expected_cases.sort();
    assert_eq!(actual_cases, expected_cases, "fixture case inventory");

    for case in FIXTURE_CASES {
        let directory = family_root.join(case);
        for file in FIXTURE_FILES {
            assert!(
                directory.join(file).is_file(),
                "missing {family}/{case}/{file}"
            );
        }

        let canonical_bytes = fs::read(directory.join("canonical.json")).expect("read canonical");
        let canonical: serde_json::Value =
            serde_json::from_slice(&canonical_bytes).expect("canonical JSON");
        assert_eq!(canonical["schemaVersion"], 1);
        assert_eq!(canonical["family"], family);
        assert_eq!(canonical["case"], *case);
        assert_eq!(
            canonical["piCommit"],
            "c49906ec77788625aacbdc53ebca6fbe65bd20f5"
        );
        assert_eq!(
            canonical["entrypoint"],
            if SIMPLE_CASES.contains(case) {
                "streamSimple"
            } else {
                "stream"
            }
        );

        let request_one =
            fs::read(directory.join("request-turn-1.body.json")).expect("read turn one");
        let request_two =
            fs::read(directory.join("request-turn-2.body.json")).expect("read turn two");
        for request in [&request_one, &request_two] {
            assert!(
                !request.is_empty(),
                "empty request body for {family}/{case}"
            );
            let ordered = parse_ordered_json(request).expect("ordered request JSON");
            assert_eq!(
                OrderedJsonWriter::to_vec(&ordered).expect("rewrite request JSON"),
                *request,
                "captured body is not canonical JSON.stringify output for {family}/{case}"
            );
        }
        if SIMPLE_CASES.contains(case) {
            assert_turn_two_retains_simple_lowering(family, case, &request_one, &request_two);
        }

        validate_header_artifact(&directory.join("request-turn-1.headers.json"));
        validate_header_artifact(&directory.join("request-turn-2.headers.json"));

        let response = fs::read(directory.join("response-turn-1.sse")).expect("read response");
        assert!(
            !response.is_empty(),
            "empty response frames for {family}/{case}"
        );
        let metadata: serde_json::Value =
            serde_json::from_slice(&fs::read(directory.join("metadata.json")).expect("metadata"))
                .expect("metadata JSON");
        assert_eq!(metadata["captureMode"], "hermetic-local-server");
        assert_eq!(metadata["credentialsUsed"], false);
        assert_eq!(metadata["secretsRedacted"], true);
        assert_eq!(metadata["requestTurnOneSha256"], sha256(&request_one));
        assert_eq!(metadata["responseTurnOneSha256"], sha256(&response));
        assert_eq!(metadata["requestTurnTwoSha256"], sha256(&request_two));

        for file in FIXTURE_FILES {
            let bytes = fs::read(directory.join(file)).expect("read fixture file");
            let text = String::from_utf8_lossy(&bytes);
            assert!(!text.contains("fixture-api-key-never-forwarded"));
            assert!(!text.to_ascii_lowercase().contains("bearer fixture-api-key"));
        }
    }

    let signed_turn_two = fs::read_to_string(
        family_root
            .join("signed-thinking-replay")
            .join("request-turn-2.body.json"),
    )
    .expect("signed turn-two body");
    let redacted_turn_two = fs::read_to_string(
        family_root
            .join("redacted-encrypted-reasoning-replay")
            .join("request-turn-2.body.json"),
    )
    .expect("redacted turn-two body");
    if family == "openai-completions" {
        assert!(signed_turn_two.contains("signed-fixture-reasoning"));
        assert!(redacted_turn_two.contains("encrypted-fixture-reasoning"));
    } else if family == "anthropic-messages" {
        assert!(signed_turn_two.contains("\"signature\":\"signed-fixture-reasoning\""));
        assert!(redacted_turn_two.contains("\"type\":\"redacted_thinking\""));
    } else {
        assert!(signed_turn_two.contains("signed-fixture-reasoning"));
        assert!(redacted_turn_two.contains("encrypted-fixture-reasoning"));
    }
}

#[test]
fn fixture_corpus_openai_completions_cases_are_complete_and_canonical() {
    validate_fixture_family("openai-completions");
}

#[test]
fn fixture_corpus_anthropic_messages_cases_are_complete_and_canonical() {
    validate_fixture_family("anthropic-messages");
}

/// Architecture v2 part 2 §10.8 mandatory OpenAI Responses capture corpus.
#[test]
fn fixture_corpus_openai_responses_cases_are_complete_and_canonical() {
    validate_fixture_family("openai-responses");
}

/// Architecture v2 part 2 §10.8 mandatory Codex Responses capture corpus.
#[test]
fn fixture_corpus_openai_codex_responses_cases_are_complete_and_canonical() {
    validate_fixture_family("openai-codex-responses");
}

#[test]
fn fixture_corpus_openai_reasoning_turn_two_contains_replay_marker() {
    pi_basis();
    let body = fs::read_to_string(
        fixture_root().join("openai-completions/signed-thinking-replay/request-turn-2.body.json"),
    )
    .expect("OpenAI signed-thinking turn-two body");
    assert!(body.contains("\"reasoning_details\""));
    assert!(body.contains("signed-fixture-reasoning"));
}

#[test]
fn fixture_corpus_anthropic_signed_turn_two_contains_replay_marker() {
    pi_basis();
    let body = fs::read_to_string(
        fixture_root().join("anthropic-messages/signed-thinking-replay/request-turn-2.body.json"),
    )
    .expect("Anthropic signed-thinking turn-two body");
    assert!(body.contains("\"type\":\"thinking\""));
    assert!(body.contains("\"signature\":\"signed-fixture-reasoning\""));
}

#[test]
fn fixture_corpus_anthropic_redacted_turn_two_contains_replay_marker() {
    pi_basis();
    let body =
        fs::read_to_string(fixture_root().join(
            "anthropic-messages/redacted-encrypted-reasoning-replay/request-turn-2.body.json",
        ))
        .expect("Anthropic redacted-thinking turn-two body");
    assert!(body.contains("\"type\":\"redacted_thinking\""));
    assert!(body.contains("redacted-fixture-reasoning"));
}

#[test]
fn fixture_credential_backed_m4_1_minimum_corpus_is_complete() {
    pi_basis();
    let root = fixture_root().join("credential-backed");
    let report = read_json(&root.join("report.json"));
    assert_eq!(report["schemaVersion"], 1);
    assert_eq!(
        report["piCommit"],
        "c49906ec77788625aacbdc53ebca6fbe65bd20f5"
    );
    assert_eq!(report["captureMode"], "credential-backed-local-proxy");
    let results = report["results"].as_array().expect("credential results");
    assert_eq!(results.len(), 8);
    assert!(results.iter().all(|result| result["status"] == "captured"));

    for family in ["openai-completions", "anthropic-messages"] {
        for case in CREDENTIAL_CASES {
            let directory = root.join(family).join(case);
            for file in FIXTURE_FILES {
                assert!(
                    directory.join(file).is_file(),
                    "missing credential-backed {family}/{case}/{file}"
                );
            }
            let canonical = read_json(&directory.join("canonical.json"));
            assert_eq!(
                canonical["piCommit"],
                "c49906ec77788625aacbdc53ebca6fbe65bd20f5"
            );
            assert_eq!(canonical["providerGeneratedValuesCapturedVerbatim"], true);
            let metadata = read_json(&directory.join("metadata.json"));
            assert_eq!(metadata["captureMode"], "credential-backed-local-proxy");
            assert_eq!(metadata["credentialsUsed"], true);
            assert_eq!(metadata["secretsRedacted"], true);
            assert!(metadata["credentialSource"].as_str().is_some());

            let request_one = fs::read(directory.join("request-turn-1.body.json"))
                .expect("credential turn-one body");
            let request_two = fs::read(directory.join("request-turn-2.body.json"))
                .expect("credential turn-two body");
            let response =
                fs::read(directory.join("response-turn-1.sse")).expect("credential response");
            assert_eq!(metadata["requestTurnOneSha256"], sha256(&request_one));
            assert_eq!(metadata["requestTurnTwoSha256"], sha256(&request_two));
            assert_eq!(metadata["responseTurnOneSha256"], sha256(&response));
            for request in [&request_one, &request_two] {
                let ordered = parse_ordered_json(request).expect("credential ordered request JSON");
                assert_eq!(
                    OrderedJsonWriter::to_vec(&ordered).expect("credential rewrite"),
                    *request
                );
            }
            validate_header_artifact(&directory.join("request-turn-1.headers.json"));
            validate_header_artifact(&directory.join("request-turn-2.headers.json"));

            for file in FIXTURE_FILES {
                let bytes = fs::read(directory.join(file)).expect("credential fixture artifact");
                let text = String::from_utf8_lossy(&bytes);
                assert!(!text.contains("\"authorization\": \"Bearer "));
            }
        }
    }
}
