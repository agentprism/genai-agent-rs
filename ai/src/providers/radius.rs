use super::radius_config::{
    DEFAULT_RADIUS_GATEWAY, RadiusResolvedModel, get_radius_models, get_radius_models_from_config,
    load_radius_gateway_config_with, normalize_radius_gateway_url,
};
use crate::api::ApiStreamOptions;
use crate::auth::helpers::{env_api_key_auth, lazy_oauth};
use crate::auth::oauth::load::load_radius_oauth;
use crate::auth::oauth::radius::RadiusOAuthOptions;
use crate::auth::resolve::{ModelsError, ModelsErrorCode};
use crate::auth::{Credential, ProviderAuth};
use crate::event_stream::{AssistantMessageEvent, AssistantMessageEventStream};
use crate::models::{
    ModelsPersistence, ModelsPublication, Provider, ProviderRef, RefreshModelsContext,
    RefreshModelsFuture,
};
use crate::models_store::ModelsStoreEntry;
use crate::types::{
    AssistantMessage, Context, ErrorStopReason, Model, SimpleStreamOptions, StopReason,
};
use crate::types::{FetchFunction, default_fetch};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RadiusProviderOptions {
    pub id: Option<String>,
    pub name: Option<String>,
    pub gateway: Option<String>,
}

struct RadiusProvider {
    id: String,
    name: String,
    gateway: String,
    auth: ProviderAuth,
    models: Arc<RwLock<Vec<Model>>>,
    fetch: Arc<dyn FetchFunction>,
}

/// Converts gateway models into crate [`Model`]s.
///
/// pi keeps every model that passes `isRadiusGatewayModel` verbatim
/// (`radius-config.ts:26-40,61-68`). `Model` is the crate-wide shape
/// (`types.rs:1994-2008`): `contextWindow`/`maxTokens` are `u64` and `input`/`cost`
/// are typed, so a gateway model whose values cannot be represented is dropped on
/// its own; the rest of the catalog is still published.
fn resolved_models(values: &[RadiusResolvedModel]) -> Vec<Model> {
    values.iter().filter_map(resolved_model).collect()
}

fn resolved_model(value: &RadiusResolvedModel) -> Option<Model> {
    let mut wire = serde_json::to_value(value).ok()?;
    let object = wire.as_object_mut()?;
    for (name, number) in [
        ("contextWindow", value.model.context_window),
        ("maxTokens", value.model.max_tokens),
    ] {
        object.insert(
            name.to_owned(),
            serde_json::Value::from(representable_u64(number)?),
        );
    }
    serde_json::from_value(wire).ok()
}

/// JS numbers pi stores as-is that a `u64` field can hold exactly.
fn representable_u64(number: f64) -> Option<u64> {
    (number.is_finite() && number >= 0.0 && number.fract() == 0.0 && number < u64::MAX as f64)
        .then_some(number as u64)
}

fn now_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |duration| duration.as_millis() as f64)
}

fn unavailable_stream(model: &Model) -> AssistantMessageEventStream {
    let mut error = AssistantMessage::pending(
        model.api.clone(),
        model.provider.clone(),
        model.id.clone(),
        now_ms(),
    );
    error.stop_reason = StopReason::Error;
    error.error_message =
        Some("The pi-messages wire protocol is excluded from this port by owner ruling".into());
    AssistantMessageEventStream::from_events(vec![AssistantMessageEvent::Error {
        reason: ErrorStopReason::Error,
        error,
    }])
}

impl Provider for RadiusProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn auth(&self) -> ProviderAuth {
        self.auth.clone()
    }

    fn get_models(&self) -> Result<Vec<Model>, ModelsError> {
        Ok(self
            .models
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone())
    }

    fn supports_refresh_models(&self) -> bool {
        true
    }

    fn refresh_models(&self, context: RefreshModelsContext) -> Option<RefreshModelsFuture> {
        let provider_id = self.id.clone();
        let gateway = self.gateway.clone();
        let models = self.models.clone();
        let fetch = self.fetch.clone();
        Some(Box::pin(async move {
            if let Some(stored) = context.stored.clone() {
                let restored = stored
                    .models
                    .into_iter()
                    .filter(|model| model.provider.as_str() == provider_id)
                    .collect::<Vec<_>>();
                let update_models = models.clone();
                if !(context.publish)(ModelsPublication {
                    persist: ModelsPersistence::Unchanged,
                    update: Some(Box::new(move || {
                        *update_models
                            .write()
                            .unwrap_or_else(PoisonError::into_inner) = restored;
                    })),
                })
                .await?
                {
                    return Ok(());
                }
            }

            if context.stored.is_none()
                && let Some(Credential::OAuth(credential)) = context.credential.as_ref()
            {
                let legacy = resolved_models(&get_radius_models(&provider_id, Some(credential)));
                if !legacy.is_empty() {
                    let persisted = legacy.clone();
                    let update_models = models.clone();
                    if !(context.publish)(ModelsPublication {
                        persist: ModelsPersistence::Write(ModelsStoreEntry {
                            models: persisted,
                            last_modified: None,
                            checked_at: Some(now_ms()),
                            etag: None,
                        }),
                        update: Some(Box::new(move || {
                            *update_models
                                .write()
                                .unwrap_or_else(PoisonError::into_inner) = legacy;
                        })),
                    })
                    .await?
                    {
                        return Ok(());
                    }
                }
            }

            if !context.allow_network || context.signal.is_aborted() {
                return Ok(());
            }
            let api_key = context
                .credential
                .as_ref()
                .and_then(|credential| match credential {
                    Credential::OAuth(credential) => Some(credential.access.clone()),
                    Credential::ApiKey(credential) => credential.key.clone(),
                });
            let config = load_radius_gateway_config_with(
                fetch,
                &gateway,
                api_key.as_deref(),
                context.signal.clone(),
            )
            .await
            .map_err(|error| ModelsError::new(ModelsErrorCode::ModelSource, error.message, None))?;
            if context.signal.is_aborted() {
                return Ok(());
            }
            let refreshed = resolved_models(&get_radius_models_from_config(&provider_id, &config));
            let persisted = refreshed.clone();
            let update_models = models;
            (context.publish)(ModelsPublication {
                persist: ModelsPersistence::Write(ModelsStoreEntry {
                    models: persisted,
                    last_modified: None,
                    checked_at: Some(now_ms()),
                    etag: None,
                }),
                update: Some(Box::new(move || {
                    *update_models
                        .write()
                        .unwrap_or_else(PoisonError::into_inner) = refreshed;
                })),
            })
            .await?;
            Ok(())
        }))
    }

    fn stream(
        &self,
        model: &Model,
        _context: &Context,
        _options: ApiStreamOptions,
    ) -> AssistantMessageEventStream {
        unavailable_stream(model)
    }

    fn stream_simple(
        &self,
        model: &Model,
        _context: &Context,
        _options: SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        unavailable_stream(model)
    }
}

pub fn radius_provider(options: RadiusProviderOptions) -> ProviderRef {
    radius_provider_with_fetch(options, default_fetch())
}

fn radius_provider_with_fetch(
    options: RadiusProviderOptions,
    fetch: Arc<dyn FetchFunction>,
) -> ProviderRef {
    let id = options.id.unwrap_or_else(|| "radius".to_owned());
    let name = options.name.unwrap_or_else(|| "Radius".to_owned());
    let gateway =
        normalize_radius_gateway_url(options.gateway.as_deref().unwrap_or(DEFAULT_RADIUS_GATEWAY));
    let oauth_name = name.clone();
    let oauth_gateway = gateway.clone();
    let oauth = lazy_oauth(
        name.clone(),
        None,
        None,
        Arc::new(move || {
            load_radius_oauth(RadiusOAuthOptions {
                name: oauth_name.clone(),
                gateway: oauth_gateway.clone(),
            })
        }),
    );
    Arc::new(RadiusProvider {
        id,
        name,
        gateway,
        auth: ProviderAuth {
            api_key: Some(env_api_key_auth(
                "Radius API key",
                vec!["RADIUS_API_KEY".to_owned()],
            )),
            oauth: Some(oauth),
        },
        models: Arc::new(RwLock::new(Vec::new())),
        fetch,
    })
}

#[cfg(test)]
mod tests {
    use super::super::radius_config::{RadiusGatewayConfig, RadiusGatewayModel};
    use super::*;
    use crate::auth::oauth::test_support::{fetch, response};
    use crate::auth::{
        ApiKeyCredential, ApiKeyCredentialType, OAuthCredential, OAuthCredentialType,
    };
    use crate::models::{ModelsPersistence, ModelsPublication};
    use crate::models_store::ModelsStoreEntry;
    use crate::utils::abort::AbortController;
    use futures::FutureExt;
    use serde_json::{Map, json};
    use std::sync::Mutex;

    fn model_json() -> serde_json::Value {
        json!({
            "id": "radius-model",
            "name": "Radius Model",
            "reasoning": false,
            "input": ["text"],
            "cost": {"input": 1, "output": 2, "cacheRead": 0, "cacheWrite": 0},
            "contextWindow": 1000,
            "maxTokens": 100
        })
    }

    fn credential() -> Credential {
        Credential::OAuth(OAuthCredential {
            kind: OAuthCredentialType::OAuth,
            refresh: "refresh".to_owned(),
            access: "access".to_owned(),
            expires: f64::MAX,
            extra: Map::from_iter([(
                "gatewayConfig".to_owned(),
                json!({"baseUrl":"https://api.radius.test","models":[model_json()]}),
            )]),
        })
    }

    /// Ports pi `src/providers/radius.ts:20-65`.
    #[tokio::test]
    async fn custom_identity_auth_and_legacy_catalog_publication_are_preserved() {
        let provider = radius_provider(RadiusProviderOptions {
            id: Some("private-radius".to_owned()),
            name: Some("Private Radius".to_owned()),
            gateway: Some("radius.example/".to_owned()),
        });
        assert_eq!(provider.id(), "private-radius");
        assert_eq!(provider.name(), "Private Radius");
        assert!(provider.auth().api_key.is_some());
        assert!(provider.auth().oauth.is_some());
        assert!(provider.get_models().expect("models").is_empty());

        let publications = Arc::new(Mutex::new(Vec::<ModelsPublication>::new()));
        let captured = publications.clone();
        provider
            .refresh_models(RefreshModelsContext {
                credential: Some(credential()),
                stored: None,
                publish: Arc::new(move |publication| {
                    captured.lock().expect("publications").push(publication);
                    async { Ok(true) }.boxed()
                }),
                allow_network: false,
                force: None,
                signal: AbortController::new().signal(),
            })
            .expect("refresh")
            .await
            .expect("refresh result");
        let mut publications = publications.lock().expect("publications");
        assert_eq!(publications.len(), 1);
        assert!(matches!(
            publications[0].persist,
            ModelsPersistence::Write(_)
        ));
        let update = publications[0].update.take().expect("update");
        drop(publications);
        update();
        let models = provider.get_models().expect("models");
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].provider.as_str(), "private-radius");
        assert_eq!(models[0].api.as_str(), "pi-messages");
        assert_eq!(models[0].base_url, "https://api.radius.test");
    }

    /// Pins the owner-ruling boundary at pi `src/providers/radius.ts:25,79-80`.
    #[tokio::test]
    async fn excluded_pi_messages_transport_fails_in_band() {
        let provider = radius_provider(Default::default());
        let resolved = resolved_models(&get_radius_models_from_config(
            "radius",
            &RadiusGatewayConfig {
                base_url: "https://api.radius.test".to_owned(),
                models: vec![serde_json::from_value(model_json()).expect("gateway model")],
            },
        ));
        let result = provider
            .stream_simple(
                &resolved[0],
                &Context::default(),
                SimpleStreamOptions::default(),
            )
            .result()
            .await
            .expect("terminal message");
        assert_eq!(result.stop_reason, StopReason::Error);
        assert!(
            result
                .error_message
                .as_deref()
                .is_some_and(|message| message.contains("owner ruling"))
        );
    }

    /// Pins pi `src/providers/radius.ts:35-77`'s restored-first then network-refreshed publication order.
    #[tokio::test]
    async fn stored_catalog_publishes_before_authenticated_network_refresh() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured_requests = requests.clone();
        let fetcher = fetch(move |request| {
            captured_requests.lock().expect("requests").push(request);
            Ok(response(
                200,
                serde_json::to_string(&json!({
                    "baseUrl": "https://api.radius.test",
                    "models": [{
                        "id": "refreshed",
                        "name": "Refreshed",
                        "reasoning": false,
                        "input": ["text"],
                        "cost": {"input": 1, "output": 2, "cacheRead": 0, "cacheWrite": 0},
                        "contextWindow": 2000,
                        "maxTokens": 200
                    }]
                }))
                .expect("response JSON"),
            ))
        });
        let provider = radius_provider_with_fetch(Default::default(), fetcher);
        let mut restored = resolved_models(&get_radius_models_from_config(
            "radius",
            &RadiusGatewayConfig {
                base_url: "https://stored.radius.test".to_owned(),
                models: vec![serde_json::from_value(model_json()).expect("stored model")],
            },
        ));
        restored[0].id = "stored".to_owned();
        let publication_order = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
        let captured_order = publication_order.clone();
        provider
            .refresh_models(RefreshModelsContext {
                credential: Some(Credential::ApiKey(ApiKeyCredential {
                    kind: ApiKeyCredentialType::ApiKey,
                    key: Some("radius-key".to_owned()),
                    env: None,
                })),
                stored: Some(ModelsStoreEntry {
                    models: restored,
                    last_modified: None,
                    checked_at: Some(1.0),
                    etag: None,
                }),
                publish: Arc::new(move |mut publication| {
                    let captured_order = captured_order.clone();
                    async move {
                        let ids = match &publication.persist {
                            ModelsPersistence::Write(entry) => {
                                entry.models.iter().map(|model| model.id.clone()).collect()
                            }
                            ModelsPersistence::Unchanged => vec!["stored".to_owned()],
                            ModelsPersistence::Delete => vec!["deleted".to_owned()],
                        };
                        if let Some(update) = publication.update.take() {
                            update();
                        }
                        captured_order.lock().expect("order").push(ids);
                        Ok(true)
                    }
                    .boxed()
                }),
                allow_network: true,
                force: None,
                signal: AbortController::new().signal(),
            })
            .expect("refresh")
            .await
            .expect("refresh result");
        assert_eq!(
            *publication_order.lock().expect("order"),
            [vec!["stored".to_owned()], vec!["refreshed".to_owned()]]
        );
        assert_eq!(
            provider
                .get_models()
                .expect("models")
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            ["refreshed"]
        );
        let requests = requests.lock().expect("requests");
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].url, "https://radius.pi.dev/v1/config");
        assert_eq!(requests[0].headers["authorization"], "Bearer radius-key");
    }

    /// pi keeps every `typeof number` value and any `input`/`cost` shape
    /// (`radius-config.ts:26-40`); the crate `Model` cannot, so only the unrepresentable
    /// model is dropped — never the catalog around it.
    #[test]
    fn unrepresentable_gateway_models_are_dropped_individually() {
        let gateway_model = |id: &str, context_window: f64, max_tokens: f64| {
            let mut model: RadiusGatewayModel =
                serde_json::from_value(model_json()).expect("gateway model");
            model.id = id.to_owned();
            model.context_window = context_window;
            model.max_tokens = max_tokens;
            model
        };
        let mut unknown_input = gateway_model("unknown-input", 1_000.0, 100.0);
        unknown_input.input = vec![serde_json::Value::String("audio".to_owned())];
        let config = RadiusGatewayConfig {
            base_url: "https://api.radius.test".to_owned(),
            models: vec![
                gateway_model("first", 1_000.0, 100.0),
                gateway_model("fractional", 1_000.5, 100.0),
                gateway_model("negative", 1_000.0, -1.0),
                gateway_model("nan", f64::NAN, 100.0),
                gateway_model("infinite", f64::INFINITY, 100.0),
                gateway_model("too-large", 18_446_744_073_709_551_616.0, 100.0),
                unknown_input,
                gateway_model("last", 2_000.0, 200.0),
            ],
        };
        let resolved = resolved_models(&get_radius_models_from_config("radius", &config));
        assert_eq!(
            resolved
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            ["first", "last"]
        );
        assert_eq!(resolved[1].context_window, 2_000.0);
        assert_eq!(resolved[1].max_tokens, 200.0);
    }

    /// pi `radius.ts:69-77` publishes whatever the gateway returned; one model the crate
    /// cannot represent must not turn that into a failed refresh.
    #[tokio::test]
    async fn network_refresh_publishes_the_representable_models_when_one_is_not() {
        let fetcher = fetch(move |_request| {
            Ok(response(
                200,
                serde_json::to_string(&json!({
                    "baseUrl": "https://api.radius.test",
                    "models": [
                        {
                            "id": "fractional",
                            "name": "Fractional",
                            "reasoning": false,
                            "input": ["text"],
                            "cost": {"input": 1, "output": 2, "cacheRead": 0, "cacheWrite": 0},
                            "contextWindow": 1000.5,
                            "maxTokens": 100
                        },
                        {
                            "id": "good",
                            "name": "Good",
                            "reasoning": false,
                            "input": ["text"],
                            "cost": {"input": 1, "output": 2, "cacheRead": 0, "cacheWrite": 0},
                            "contextWindow": 2000,
                            "maxTokens": 200
                        }
                    ]
                }))
                .expect("response JSON"),
            ))
        });
        let provider = radius_provider_with_fetch(Default::default(), fetcher);
        let published = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
        let captured = published.clone();
        provider
            .refresh_models(RefreshModelsContext {
                credential: Some(Credential::ApiKey(ApiKeyCredential {
                    kind: ApiKeyCredentialType::ApiKey,
                    key: Some("radius-key".to_owned()),
                    env: None,
                })),
                stored: None,
                publish: Arc::new(move |mut publication| {
                    let captured = captured.clone();
                    async move {
                        if let ModelsPersistence::Write(entry) = &publication.persist {
                            captured
                                .lock()
                                .expect("published")
                                .push(entry.models.iter().map(|model| model.id.clone()).collect());
                        }
                        if let Some(update) = publication.update.take() {
                            update();
                        }
                        Ok(true)
                    }
                    .boxed()
                }),
                allow_network: true,
                force: None,
                signal: AbortController::new().signal(),
            })
            .expect("refresh")
            .await
            .expect("refresh result");
        assert_eq!(
            *published.lock().expect("published"),
            [vec!["good".to_owned()]]
        );
        assert_eq!(
            provider
                .get_models()
                .expect("models")
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            ["good"]
        );
    }
}
