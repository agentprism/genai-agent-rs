use super::*;
use crate::types::{Api, ModelCost, ModelInput, ProviderId, ProviderRequestOptions};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

fn model(id: &str, base_url: &str) -> Model {
    Model {
        id: id.to_owned(),
        name: id.to_owned(),
        api: Api::from("google-vertex"),
        provider: ProviderId::from("google-vertex"),
        base_url: base_url.to_owned(),
        reasoning: true,
        thinking_level_map: None,
        input: vec![ModelInput::Text],
        cost: ModelCost::default(),
        context_window: 128_000.0,
        max_tokens: 4_096.0,
        sampling_params: None,
        headers: None,
        compat: None,
    }
}

fn options(api_key: Option<&str>) -> GoogleVertexOptions {
    GoogleVertexOptions {
        stream: StreamOptions {
            request: ProviderRequestOptions {
                api_key: api_key.map(str::to_owned),
                ..ProviderRequestOptions::default()
            },
            ..StreamOptions::default()
        },
        ..GoogleVertexOptions::default()
    }
}

/// Ports the pure resolution cases from pi `test/google-vertex-api-key-resolution.test.ts:90-157`.
#[test]
fn placeholder_and_marker_keys_fall_back_to_adc() {
    assert_eq!(resolve_api_key(Some(&options(None))), None);
    assert_eq!(
        resolve_api_key(Some(&options(Some("<authenticated>")))),
        None
    );
    assert_eq!(
        resolve_api_key(Some(&options(Some(GCP_VERTEX_CREDENTIALS_MARKER)))),
        None
    );
    assert_eq!(
        resolve_api_key(Some(&options(Some("  AIzaSyExample  ")))).as_deref(),
        Some("AIzaSyExample")
    );
}

/// Ports pi `src/api/google-vertex.ts:403-455` and the custom URL assertions in
/// `test/google-vertex-api-key-resolution.test.ts:184-242`.
#[test]
fn vertex_project_location_and_custom_url_resolution_match_pi() {
    let mut options = options(None);
    options.project = Some("explicit-project".to_owned());
    options.location = Some("global".to_owned());
    options.stream.request.env = Some(ProviderEnv::from([
        ("GOOGLE_CLOUD_PROJECT".to_owned(), "env-project".to_owned()),
        ("GCLOUD_PROJECT".to_owned(), "gcloud-project".to_owned()),
        ("GOOGLE_CLOUD_LOCATION".to_owned(), "us-central1".to_owned()),
    ]));
    assert_eq!(
        resolve_project(Some(&options)).as_deref(),
        Ok("explicit-project")
    );
    assert_eq!(resolve_location(Some(&options)).as_deref(), Ok("global"));
    assert_eq!(
        resolve_custom_base_url(" https://proxy.example.com ").as_deref(),
        Some("https://proxy.example.com")
    );
    assert_eq!(
        resolve_custom_base_url("https://{location}.example.com"),
        None
    );
    assert!(base_url_includes_api_version(
        "https://proxy.example.com/v1/projects/p/locations/global"
    ));
    assert!(base_url_includes_api_version("relative/v1beta3/models"));
    assert!(!base_url_includes_api_version(
        "https://proxy.example.com/models"
    ));
    let custom = model("gemini-3-flash-preview", "https://proxy.example.com");
    assert_eq!(
        vertex_api_key_base_url(&custom),
        "https://proxy.example.com/v1/"
    );
    assert_eq!(
        vertex_adc_base_url(&custom, "ignored-project", "ignored-location"),
        "https://proxy.example.com/v1/"
    );
    let versioned = model("gemini-3-flash-preview", "https://proxy.example.com/v1beta");
    assert_eq!(
        vertex_api_key_base_url(&versioned),
        "https://proxy.example.com/v1beta/"
    );
    let generated = model(
        "gemini-3-flash-preview",
        "https://{location}-aiplatform.googleapis.com",
    );
    assert_eq!(
        vertex_adc_base_url(&generated, "project-id", "us-central1"),
        "https://us-central1-aiplatform.googleapis.com/v1/projects/project-id/locations/us-central1/"
    );
    assert_eq!(
        vertex_adc_base_url(&generated, "project-id", "global"),
        "https://aiplatform.googleapis.com/v1/projects/project-id/locations/global/"
    );
    assert_eq!(
        vertex_adc_base_url(&generated, "project-id", "eu"),
        "https://aiplatform.eu.rep.googleapis.com/v1/projects/project-id/locations/eu/"
    );
}

/// Pins the Vertex-only budget distinction at pi `src/api/google-vertex.ts:568-598`.
#[test]
fn vertex_budget_does_not_apply_the_gemini_flash_lite_minimum() {
    assert_eq!(
        vertex_google_budget(
            &model("gemini-2.5-flash-lite", ""),
            ResolvedGoogleThinkingLevel::Minimal,
            None
        ),
        128.0
    );
    assert_eq!(
        vertex_google_budget(
            &model("unrecognized", ""),
            ResolvedGoogleThinkingLevel::High,
            None
        ),
        -1.0
    );
}

/// Pins pi `src/api/google-vertex.ts:101-111`: Vertex client configuration errors precede
/// payload construction and `onPayload`.
#[tokio::test]
async fn vertex_client_configuration_errors_precede_payload_hooks() {
    let payload_calls = Arc::new(AtomicUsize::new(0));
    let callback_calls = payload_calls.clone();
    let mut options = options(Some("test-key"));
    options.stream.request.on_payload = Some(Arc::new(move |_, _| {
        callback_calls.fetch_add(1, Ordering::Relaxed);
        Box::pin(async { Err("payload hook should not run".to_owned()) })
    }));

    let message = stream(
        &model("gemini-3-flash-preview", "not a valid URL"),
        &Context::default(),
        options,
    )
    .result()
    .await
    .expect("terminal result");

    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(payload_calls.load(Ordering::Relaxed), 0);
    assert!(
        message
            .error_message
            .as_deref()
            .is_some_and(|error| !error.contains("payload hook should not run"))
    );
}
