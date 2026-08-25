use pi_ai::*;
use pi_ai_github_copilot::{
    GitHubCopilotOAuth, LocalGitHubCopilotOAuth, filter_entitled_models, local_provider, models,
    provider,
};
use pi_ai_provider_common::{LocalProviderInputs, ProviderInputs};
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;
use url::Url;

#[path = "../../fixtures/oauth_support.rs"]
mod support;
use support::*;

fn credential() -> OAuthCredential {
    OAuthCredential {
        access: SecretString::new("expired-copilot"),
        refresh: SecretString::new("github-token"),
        expires_at: Timestamp::default(),
        extra: ProviderOAuthExtra::GitHubCopilot {
            api_endpoint: Url::parse("https://api.individual.githubcopilot.com").unwrap(),
            account_id: None,
            enterprise_url: None,
            available_model_ids: None,
        },
    }
}

fn responses() -> [HttpScriptedResponse; 2] {
    [
        HttpScriptedResponse::json(
            200,
            r#"{"token":"copilot-token;proxy-ep=proxy.individual.githubcopilot.com","expires_at":2000000000}"#,
        ),
        HttpScriptedResponse::json(
            200,
            r#"{"data":[{"id":"claude-haiku-4.5","model_picker_enabled":true,"policy":{"state":"enabled"},"capabilities":{"supports":{"tool_calls":true}}},{"id":"model-without-tools","model_picker_enabled":true,"policy":{"state":"enabled"},"capabilities":{"supports":{"tool_calls":false}}}]}"#,
        ),
    ]
}

#[test]
fn github_copilot_oauth_entitlements_pi_exact_send_and_local() {
    // Pi basis: packages/ai/test/github-copilot-oauth.test.ts and
    // providers/github-copilot.ts: token exchange, proxy endpoint derivation,
    // `/models` entitlement filtering, tool-call capability exclusion, and
    // credential-scoped catalog narrowing.
    let send_transport = Arc::new(ScriptedTransport::new(responses()));
    let oauth = GitHubCopilotOAuth::new(send_transport.clone());
    let refreshed =
        futures_executor::block_on(oauth.refresh(credential(), CancellationToken::new())).unwrap();
    let ProviderOAuthExtra::GitHubCopilot {
        api_endpoint,
        available_model_ids,
        ..
    } = &refreshed.extra
    else {
        panic!("Copilot metadata")
    };
    assert_eq!(
        api_endpoint.as_str(),
        "https://api.individual.githubcopilot.com/"
    );
    assert_eq!(
        available_model_ids.as_ref().unwrap(),
        &[ModelId::new("claude-haiku-4.5")]
    );
    let seen = send_transport.seen.lock().unwrap();
    assert_eq!(
        seen[0].url,
        "https://api.github.com/copilot_internal/v2/token"
    );
    assert_eq!(
        seen[1].url,
        "https://api.individual.githubcopilot.com/models"
    );
    assert_eq!(seen[1].headers["x-github-api-version"], "2026-06-01");
    drop(seen);
    let narrowed = filter_entitled_models(&models().unwrap(), Some(&Credential::OAuth(refreshed)));
    assert_eq!(narrowed.len(), 1);
    assert_eq!(
        narrowed[0].common.model_ref.model,
        ModelId::new("claude-haiku-4.5")
    );

    let local_transport = Rc::new(ScriptedTransport::new(responses()));
    let oauth = LocalGitHubCopilotOAuth::new(local_transport.clone());
    let refreshed =
        futures_executor::block_on(oauth.refresh(credential(), CancellationToken::new())).unwrap();
    assert!(matches!(
        refreshed.extra,
        ProviderOAuthExtra::GitHubCopilot {
            available_model_ids: Some(ref ids),
            ..
        } if ids == &[ModelId::new("claude-haiku-4.5")]
    ));
    assert_eq!(local_transport.seen.lock().unwrap().len(), 2);

    // Pi basis: `retries a throttled policy update after Retry-After` and
    // `stops policy updates and persists authentication when the retry delay
    // exceeds the login budget` in github-copilot-oauth.test.ts.
    let mut throttled = HttpScriptedResponse::json(429, r#"{"error":"too many requests"}"#);
    throttled.headers.insert(
        http::header::RETRY_AFTER,
        http::HeaderValue::from_static("0"),
    );
    let send_transport = Arc::new(ScriptedTransport::new([
        HttpScriptedResponse::json(
            200,
            r#"{"device_code":"device","user_code":"ABCD","verification_uri":"https://github.com/login/device","interval":0,"expires_in":900}"#,
        ),
        HttpScriptedResponse::json(200, r#"{"access_token":"github-token"}"#),
        HttpScriptedResponse::json(
            200,
            r#"{"token":"copilot-token;proxy-ep=proxy.individual.githubcopilot.com","expires_at":2000000000}"#,
        ),
        HttpScriptedResponse::json(
            200,
            r#"{"data":[{"id":"claude-haiku-4.5","model_picker_enabled":true,"policy":{"state":"unconfigured"},"capabilities":{"supports":{"tool_calls":true}}}]}"#,
        ),
        throttled,
        HttpScriptedResponse::json(200, ""),
    ]));
    let oauth = GitHubCopilotOAuth::new(send_transport.clone());
    let interaction = Arc::new(RecordingInteraction::with_answers([AuthAnswer::Text(
        String::new(),
    )]));
    let credential =
        futures_executor::block_on(oauth.login(interaction, CancellationToken::new())).unwrap();
    assert!(matches!(
        credential.extra,
        ProviderOAuthExtra::GitHubCopilot {
            available_model_ids: Some(ref ids),
            ..
        } if ids == &[ModelId::new("claude-haiku-4.5")]
    ));
    assert_eq!(send_transport.seen.lock().unwrap().len(), 6);

    let mut throttled = HttpScriptedResponse::json(429, r#"{"error":"too many requests"}"#);
    throttled.headers.insert(
        http::header::RETRY_AFTER,
        http::HeaderValue::from_static("0"),
    );
    let send_transport = Arc::new(ScriptedTransport::new([
        HttpScriptedResponse::json(
            200,
            r#"{"device_code":"device","user_code":"ABCD","verification_uri":"https://github.com/login/device","interval":0,"expires_in":900}"#,
        ),
        HttpScriptedResponse::json(200, r#"{"access_token":"github-token"}"#),
        HttpScriptedResponse::json(
            200,
            r#"{"token":"copilot-token;proxy-ep=proxy.individual.githubcopilot.com","expires_at":2000000000}"#,
        ),
        throttled,
        HttpScriptedResponse::json(
            200,
            r#"{"data":[{"id":"gpt-4.1","model_picker_enabled":true,"policy":{"state":"enabled"},"capabilities":{"supports":{"tool_calls":true}}}]}"#,
        ),
    ]));
    let oauth = GitHubCopilotOAuth::new(send_transport.clone());
    futures_executor::block_on(oauth.login(
        Arc::new(RecordingInteraction::with_answers([AuthAnswer::Text(
            String::new(),
        )])),
        CancellationToken::new(),
    ))
    .unwrap();
    assert_eq!(send_transport.seen.lock().unwrap().len(), 5);
}

fn enterprise_credential() -> OAuthCredential {
    OAuthCredential {
        access: SecretString::new("expired-enterprise-copilot"),
        refresh: SecretString::new("enterprise-github-token"),
        expires_at: Timestamp::default(),
        extra: ProviderOAuthExtra::GitHubCopilot {
            api_endpoint: Url::parse("https://stale.example.invalid").unwrap(),
            account_id: Some("account-identity".into()),
            enterprise_url: Some("enterprise.example".into()),
            available_model_ids: None,
        },
    }
}

fn enterprise_responses() -> [HttpScriptedResponse; 2] {
    [
        HttpScriptedResponse::json(
            200,
            r#"{"token":"enterprise-copilot-token","expires_at":2000000000}"#,
        ),
        HttpScriptedResponse::json(200, r#"{"data":[]}"#),
    ]
}

fn assert_enterprise_metadata(credential: &OAuthCredential) {
    let ProviderOAuthExtra::GitHubCopilot {
        api_endpoint,
        account_id,
        enterprise_url,
        ..
    } = &credential.extra
    else {
        panic!("Copilot enterprise metadata")
    };
    assert_eq!(
        api_endpoint.as_str(),
        "https://copilot-api.enterprise.example/"
    );
    assert_eq!(account_id.as_deref(), Some("account-identity"));
    assert_eq!(enterprise_url.as_deref(), Some("enterprise.example"));
}

#[test]
fn github_copilot_enterprise_metadata_refresh_and_auth_send_and_local_pi_exact() {
    // Architecture v2 part 2 §6.6, §9.2, and §10.7; Pi basis:
    // packages/ai/src/auth/oauth/github-copilot.ts:341-347,487-505 keeps
    // enterpriseUrl distinct and uses it for refresh and toAuth.
    let send_transport = Arc::new(ScriptedTransport::new(enterprise_responses()));
    let oauth = GitHubCopilotOAuth::new(send_transport.clone());
    let refreshed = futures_executor::block_on(OAuthAuth::refresh(
        &oauth,
        enterprise_credential(),
        CancellationToken::new(),
    ))
    .unwrap();
    assert_enterprise_metadata(&refreshed);
    let auth = futures_executor::block_on(OAuthAuth::to_auth(&oauth, &refreshed)).unwrap();
    assert_eq!(
        auth.base_url.expect("Send enterprise auth base").as_str(),
        "https://copilot-api.enterprise.example/"
    );
    let seen = send_transport.seen.lock().unwrap();
    assert_eq!(
        seen[0].url,
        "https://api.enterprise.example/copilot_internal/v2/token"
    );
    assert_eq!(seen[1].url, "https://copilot-api.enterprise.example/models");
    drop(seen);

    let local_transport = Rc::new(ScriptedTransport::new(enterprise_responses()));
    let oauth = LocalGitHubCopilotOAuth::new(local_transport.clone());
    let refreshed = futures_executor::block_on(LocalOAuthAuth::refresh(
        &oauth,
        enterprise_credential(),
        CancellationToken::new(),
    ))
    .unwrap();
    assert_enterprise_metadata(&refreshed);
    let auth = futures_executor::block_on(LocalOAuthAuth::to_auth(&oauth, &refreshed)).unwrap();
    assert_eq!(
        auth.base_url.expect("Local enterprise auth base").as_str(),
        "https://copilot-api.enterprise.example/"
    );
    let seen = local_transport.seen.lock().unwrap();
    assert_eq!(
        seen[0].url,
        "https://api.enterprise.example/copilot_internal/v2/token"
    );
    assert_eq!(seen[1].url, "https://copilot-api.enterprise.example/models");
}

fn request_context() -> Context {
    Context {
        schema_version: 1,
        system_prompt: None,
        messages: vec![Message::User(UserMessage {
            id: MessageId::new("copilot-user"),
            content: vec![ContentBlock::Text {
                id: ContentBlockId::new("copilot-text"),
                text: "hello".to_owned(),
            }],
            timestamp: Timestamp::default(),
        })],
        tools: Vec::new(),
    }
}

fn assistant_context(model: &ModelRef, api: &str) -> Context {
    let mut context = request_context();
    context.messages.push(Message::Assistant(AssistantMessage {
        id: MessageId::new("copilot-assistant"),
        provider: model.provider.clone(),
        api: ApiId::new(api),
        requested_model: model.model.clone(),
        response_model: None,
        response_id: None,
        deferred: None,
        end_turn: None,
        diagnostics: Vec::new(),
        content: vec![ContentBlock::Text {
            id: ContentBlockId::new("copilot-assistant-text"),
            text: "follow up".to_owned(),
        }],
        replay: ReplayEnvelope::new(ReplayScope::new(
            model.provider.clone(),
            ApiId::new(api),
            model.model.clone(),
            model.model.clone(),
        )),
        usage: Usage::zero(UsageSource::ProviderReported),
        cost: None,
        finish: AssistantFinish {
            reason: AssistantFinishReason::Stop,
            raw_provider_reason: None,
            error: None,
        },
        timestamp: Timestamp::default(),
    }));
    context
}

fn image_context() -> Context {
    let mut context = request_context();
    let Message::User(user) = &mut context.messages[0] else {
        unreachable!()
    };
    user.content = vec![ContentBlock::Image {
        id: ContentBlockId::new("copilot-image"),
        data: "aW1hZ2U=".to_owned(),
        mime_type: "image/png".to_owned(),
    }];
    context
}

const COPILOT_APIS: [&str; 3] = [
    "anthropic-messages",
    "openai-completions",
    "openai-responses",
];

fn copilot_model_refs(catalog: &[ModelDescriptor]) -> [(&'static str, ModelRef); 3] {
    COPILOT_APIS.map(|api| {
        let model = catalog
            .iter()
            .find(|model| model.api.api_id() == ApiId::new(api))
            .unwrap_or_else(|| panic!("missing Copilot {api} model"));
        (api, model.common.model_ref.clone())
    })
}

fn context_for_api(api: &str, model: &ModelRef) -> Context {
    match api {
        "openai-completions" => assistant_context(model, api),
        "openai-responses" => image_context(),
        _ => request_context(),
    }
}

fn add_conflicting_send_model_headers(registration: &mut ProviderRegistration) {
    let catalog: Vec<ModelDescriptor> = registration
        .catalog
        .snapshot()
        .iter()
        .cloned()
        .map(|mut model| {
            model
                .common
                .headers
                .insert("X-Initiator".to_owned(), Some("model".to_owned()));
            model
                .common
                .headers
                .insert("Openai-Intent".to_owned(), Some("model-intent".to_owned()));
            model
        })
        .collect();
    registration.catalog = Arc::new(StaticModelCatalog::new(catalog));
}

fn add_conflicting_local_model_headers(registration: &mut LocalProviderRegistration) {
    let catalog: Vec<ModelDescriptor> = registration
        .catalog
        .snapshot()
        .iter()
        .cloned()
        .map(|mut model| {
            model
                .common
                .headers
                .insert("X-Initiator".to_owned(), Some("model".to_owned()));
            model
                .common
                .headers
                .insert("Openai-Intent".to_owned(), Some("model-intent".to_owned()));
            model
        })
        .collect();
    registration.catalog = Rc::new(LocalStaticModelCatalog::new(catalog));
}

struct DeleteCopilotContextHeaders;

fn delete_copilot_context_headers(headers: &mut http::HeaderMap) {
    for name in ["x-initiator", "openai-intent", "copilot-vision-request"] {
        headers.remove(name);
    }
}

impl HeaderTransform for DeleteCopilotContextHeaders {
    fn transform<'a>(
        &'a self,
        _context: HeaderTransformContext<'a>,
        headers: &'a mut http::HeaderMap,
    ) -> SendBoxFuture<'a, Result<(), MiddlewareError>> {
        delete_copilot_context_headers(headers);
        Box::pin(async { Ok(()) })
    }
}

impl LocalHeaderTransform for DeleteCopilotContextHeaders {
    fn transform<'a>(
        &'a self,
        _context: HeaderTransformContext<'a>,
        headers: &'a mut http::HeaderMap,
    ) -> LocalBoxFuture<'a, Result<(), MiddlewareError>> {
        delete_copilot_context_headers(headers);
        Box::pin(async { Ok(()) })
    }
}

#[test]
fn github_copilot_request_context_headers_all_families_send_and_local_pi_exact() {
    // Pi basis: api/github-copilot-headers.ts plus the call sites in
    // anthropic-messages.ts, openai-completions.ts, and openai-responses.ts.
    let error_responses =
        || (0..3).map(|_| HttpScriptedResponse::json(400, r#"{"error":"fixture"}"#));

    let send_transport = Arc::new(ScriptedTransport::new(error_responses()));
    let mut registration = provider(ProviderInputs {
        http: send_transport.clone(),
        environment: BTreeMap::new(),
    })
    .unwrap();
    add_conflicting_send_model_headers(&mut registration);
    let catalog = registration.catalog.snapshot();
    let model_refs = copilot_model_refs(&catalog);
    let models = Models::builder()
        .auth_context(Arc::new(MapAuthContext::new(
            BTreeMap::from([(
                "COPILOT_GITHUB_TOKEN".to_owned(),
                "copilot-fixture".to_owned(),
            )]),
            Vec::<String>::new(),
        )))
        .provider(registration)
        .build()
        .unwrap();
    for (api, model) in model_refs {
        let context = context_for_api(api, &model);
        assert!(
            futures_executor::block_on(ModelRuntime::stream(
                &models,
                ModelRequest {
                    model,
                    context,
                    options: SimpleGenerationOptions::default(),
                },
                CancellationToken::new(),
            ))
            .is_err()
        );
    }
    let send_seen = send_transport.seen.lock().unwrap();
    assert_eq!(send_seen.len(), 3);
    for (index, request) in send_seen.iter().enumerate() {
        assert_eq!(
            request.headers["x-initiator"],
            if index == 1 { "agent" } else { "user" }
        );
        assert_eq!(request.headers["openai-intent"], "conversation-edits");
        assert_eq!(
            request.headers[http::header::AUTHORIZATION],
            "Bearer copilot-fixture"
        );
        assert_eq!(
            request.headers.contains_key("copilot-vision-request"),
            index == 2
        );
    }
    drop(send_seen);

    let local_transport = Rc::new(ScriptedTransport::new(error_responses()));
    let mut registration = local_provider(LocalProviderInputs {
        http: local_transport.clone(),
        environment: BTreeMap::new(),
    })
    .unwrap();
    add_conflicting_local_model_headers(&mut registration);
    let catalog = registration.catalog.snapshot();
    let model_refs = copilot_model_refs(&catalog);
    let models = LocalModels::builder()
        .auth_context(Rc::new(MapAuthContext::new(
            BTreeMap::from([(
                "COPILOT_GITHUB_TOKEN".to_owned(),
                "local-copilot-fixture".to_owned(),
            )]),
            Vec::<String>::new(),
        )))
        .provider(registration)
        .build()
        .unwrap();
    for (api, model) in model_refs {
        let context = context_for_api(api, &model);
        assert!(
            futures_executor::block_on(LocalModelRuntime::stream(
                &models,
                ModelRequest {
                    model,
                    context,
                    options: SimpleGenerationOptions::default(),
                },
                CancellationToken::new(),
            ))
            .is_err()
        );
    }
    let local_seen = local_transport.seen.lock().unwrap();
    assert_eq!(local_seen.len(), 3);
    for (index, request) in local_seen.iter().enumerate() {
        assert_eq!(
            request.headers["x-initiator"],
            if index == 1 { "agent" } else { "user" }
        );
        assert_eq!(request.headers["openai-intent"], "conversation-edits");
        assert_eq!(
            request.headers[http::header::AUTHORIZATION],
            "Bearer local-copilot-fixture"
        );
        assert_eq!(
            request.headers.contains_key("copilot-vision-request"),
            index == 2
        );
    }
}

#[test]
fn github_copilot_explicit_headers_override_and_delete_dynamic_send_and_local_pi_exact() {
    // Architecture v2 part 2 §2.6 and §10.4; Pi basis:
    // api/anthropic-messages.ts, api/openai-completions.ts, and
    // api/openai-responses.ts apply option headers after Copilot dynamic
    // headers, including null deletion through mergeClientHeaders.
    let error_responses =
        || (0..3).map(|_| HttpScriptedResponse::json(400, r#"{"error":"fixture"}"#));
    let explicit = SimpleGenerationOptions {
        headers: BTreeMap::from([
            ("X-Initiator".to_owned(), Some("explicit".to_owned())),
            ("Openai-Intent".to_owned(), None),
            ("Copilot-Vision-Request".to_owned(), None),
        ]),
        ..Default::default()
    };

    let send_transport = Arc::new(ScriptedTransport::new(error_responses()));
    let registration = provider(ProviderInputs {
        http: send_transport.clone(),
        environment: BTreeMap::new(),
    })
    .unwrap();
    let model_refs = copilot_model_refs(&registration.catalog.snapshot());
    let models = Models::builder()
        .auth_context(Arc::new(MapAuthContext::new(
            BTreeMap::from([(
                "COPILOT_GITHUB_TOKEN".to_owned(),
                "copilot-fixture".to_owned(),
            )]),
            Vec::<String>::new(),
        )))
        .provider(registration)
        .build()
        .unwrap();
    for (api, model) in model_refs {
        assert!(
            futures_executor::block_on(ModelRuntime::stream(
                &models,
                ModelRequest {
                    context: context_for_api(api, &model),
                    model,
                    options: explicit.clone(),
                },
                CancellationToken::new(),
            ))
            .is_err()
        );
    }
    for request in send_transport.seen.lock().unwrap().iter() {
        assert_eq!(request.headers["x-initiator"], "explicit");
        assert!(!request.headers.contains_key("openai-intent"));
        assert!(!request.headers.contains_key("copilot-vision-request"));
    }

    let local_transport = Rc::new(ScriptedTransport::new(error_responses()));
    let registration = local_provider(LocalProviderInputs {
        http: local_transport.clone(),
        environment: BTreeMap::new(),
    })
    .unwrap();
    let model_refs = copilot_model_refs(&registration.catalog.snapshot());
    let models = LocalModels::builder()
        .auth_context(Rc::new(MapAuthContext::new(
            BTreeMap::from([(
                "COPILOT_GITHUB_TOKEN".to_owned(),
                "local-copilot-fixture".to_owned(),
            )]),
            Vec::<String>::new(),
        )))
        .provider(registration)
        .build()
        .unwrap();
    for (api, model) in model_refs {
        assert!(
            futures_executor::block_on(LocalModelRuntime::stream(
                &models,
                ModelRequest {
                    context: context_for_api(api, &model),
                    model,
                    options: explicit.clone(),
                },
                CancellationToken::new(),
            ))
            .is_err()
        );
    }
    for request in local_transport.seen.lock().unwrap().iter() {
        assert_eq!(request.headers["x-initiator"], "explicit");
        assert!(!request.headers.contains_key("openai-intent"));
        assert!(!request.headers.contains_key("copilot-vision-request"));
    }
}

#[test]
fn github_copilot_header_transform_can_delete_dynamic_send_and_local_pi_exact() {
    // Architecture v2 part 2 §2.6 and §10.4
    // `headers_transform_can_delete_default`; Pi basis: Models' final
    // HeaderTransform replacement plus Copilot header call sites in all three
    // API families.
    let error_responses =
        || (0..3).map(|_| HttpScriptedResponse::json(400, r#"{"error":"fixture"}"#));

    let send_transport = Arc::new(ScriptedTransport::new(error_responses()));
    let registration = provider(ProviderInputs {
        http: send_transport.clone(),
        environment: BTreeMap::new(),
    })
    .unwrap();
    let model_refs = copilot_model_refs(&registration.catalog.snapshot());
    let models = Models::builder()
        .auth_context(Arc::new(MapAuthContext::new(
            BTreeMap::from([(
                "COPILOT_GITHUB_TOKEN".to_owned(),
                "copilot-fixture".to_owned(),
            )]),
            Vec::<String>::new(),
        )))
        .provider(registration)
        .header_transform(Arc::new(DeleteCopilotContextHeaders))
        .build()
        .unwrap();
    for (api, model) in model_refs {
        assert!(
            futures_executor::block_on(ModelRuntime::stream(
                &models,
                ModelRequest {
                    context: context_for_api(api, &model),
                    model,
                    options: SimpleGenerationOptions::default(),
                },
                CancellationToken::new(),
            ))
            .is_err()
        );
    }
    for request in send_transport.seen.lock().unwrap().iter() {
        assert!(!request.headers.contains_key("x-initiator"));
        assert!(!request.headers.contains_key("openai-intent"));
        assert!(!request.headers.contains_key("copilot-vision-request"));
    }

    let local_transport = Rc::new(ScriptedTransport::new(error_responses()));
    let registration = local_provider(LocalProviderInputs {
        http: local_transport.clone(),
        environment: BTreeMap::new(),
    })
    .unwrap();
    let model_refs = copilot_model_refs(&registration.catalog.snapshot());
    let models = LocalModels::builder()
        .auth_context(Rc::new(MapAuthContext::new(
            BTreeMap::from([(
                "COPILOT_GITHUB_TOKEN".to_owned(),
                "local-copilot-fixture".to_owned(),
            )]),
            Vec::<String>::new(),
        )))
        .provider(registration)
        .header_transform(Rc::new(DeleteCopilotContextHeaders))
        .build()
        .unwrap();
    for (api, model) in model_refs {
        assert!(
            futures_executor::block_on(LocalModelRuntime::stream(
                &models,
                ModelRequest {
                    context: context_for_api(api, &model),
                    model,
                    options: SimpleGenerationOptions::default(),
                },
                CancellationToken::new(),
            ))
            .is_err()
        );
    }
    for request in local_transport.seen.lock().unwrap().iter() {
        assert!(!request.headers.contains_key("x-initiator"));
        assert!(!request.headers.contains_key("openai-intent"));
        assert!(!request.headers.contains_key("copilot-vision-request"));
    }
}
