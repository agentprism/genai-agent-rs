use crate::auth::oauth::{request, send_http};
use crate::auth::{AuthError, OAuthCredential};
use crate::types::{AbortSignal, FetchFunction, default_fetch};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::sync::Arc;

pub const DEFAULT_RADIUS_GATEWAY: &str = "https://radius.pi.dev";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RadiusGatewayModel {
    pub id: String,
    pub name: String,
    pub reasoning: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level_map: Option<Value>,
    pub input: Vec<Value>,
    pub cost: Map<String, Value>,
    pub context_window: f64,
    pub max_tokens: f64,
    #[serde(default, flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RadiusGatewayConfig {
    pub base_url: String,
    pub models: Vec<RadiusGatewayModel>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RadiusResolvedModel {
    #[serde(flatten)]
    pub model: RadiusGatewayModel,
    pub api: String,
    pub provider: String,
    pub base_url: String,
}

fn is_radius_gateway_model(value: &Value) -> bool {
    let Some(model) = value.as_object() else {
        return false;
    };
    model.get("id").is_some_and(Value::is_string)
        && model.get("name").is_some_and(Value::is_string)
        && model.get("reasoning").is_some_and(Value::is_boolean)
        && model.get("input").is_some_and(Value::is_array)
        && model.get("cost").is_some_and(Value::is_object)
        && model.get("contextWindow").is_some_and(Value::is_number)
        && model.get("maxTokens").is_some_and(Value::is_number)
}

fn sanitize_radius_gateway_config(config: &Value) -> Option<RadiusGatewayConfig> {
    let config = config.as_object()?;
    let base_url = config.get("baseUrl")?.as_str()?.to_owned();
    let models = config.get("models")?.as_array()?;
    Some(RadiusGatewayConfig {
        base_url,
        models: models
            .iter()
            .filter(|model| is_radius_gateway_model(model))
            .filter_map(|model| serde_json::from_value(model.clone()).ok())
            .collect(),
    })
}

pub fn normalize_radius_gateway_url(value: &str) -> String {
    let with_scheme = if value
        .get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("http://"))
        || value
            .get(..8)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("https://"))
    {
        value.to_owned()
    } else {
        format!("https://{value}")
    };
    with_scheme.trim_end_matches('/').to_owned()
}

pub fn get_radius_credential_config(
    credential: Option<&OAuthCredential>,
) -> Option<RadiusGatewayConfig> {
    sanitize_radius_gateway_config(credential?.extra.get("gatewayConfig")?)
}

pub fn get_radius_models_from_config(
    provider_id: &str,
    config: &RadiusGatewayConfig,
) -> Vec<RadiusResolvedModel> {
    config
        .models
        .iter()
        .cloned()
        .map(|mut model| {
            for key in ["api", "provider", "baseUrl"] {
                model.extra.remove(key);
            }
            RadiusResolvedModel {
                model,
                api: "pi-messages".to_owned(),
                provider: provider_id.to_owned(),
                base_url: config.base_url.clone(),
            }
        })
        .collect()
}

pub fn get_radius_models(
    provider_id: &str,
    credential: Option<&OAuthCredential>,
) -> Vec<RadiusResolvedModel> {
    get_radius_credential_config(credential).map_or_else(Vec::new, |config| {
        get_radius_models_from_config(provider_id, &config)
    })
}

fn truncate_http_body(body: &str) -> String {
    let trimmed = body.trim();
    let utf16 = trimmed.encode_utf16().collect::<Vec<_>>();
    if utf16.len() > 512 {
        format!("{}…", String::from_utf16_lossy(&utf16[..512]))
    } else {
        trimmed.to_owned()
    }
}

pub(crate) async fn load_radius_gateway_config_with(
    fetch: Arc<dyn FetchFunction>,
    gateway: &str,
    api_key: Option<&str>,
    signal: Arc<dyn AbortSignal>,
) -> Result<RadiusGatewayConfig, AuthError> {
    let mut headers = vec![("accept".to_owned(), "application/json".to_owned())];
    if let Some(api_key) = api_key.filter(|key| !key.is_empty()) {
        headers.push(("authorization".to_owned(), format!("Bearer {api_key}")));
    }
    let url = url::Url::parse(gateway)
        .and_then(|gateway| gateway.join("/v1/config"))
        .map_err(|error| AuthError::new(error.to_string()))?
        .to_string();
    let response = send_http(
        fetch,
        request("GET", url, headers, Vec::new(), signal),
        None,
    )
    .await
    .map_err(|error| AuthError::new(error.to_string()))?;
    if !response.ok() {
        return Err(AuthError::new(format!(
            "Could not load Radius config from {gateway}: {}: {}",
            response.status,
            truncate_http_body(&response.body)
        )));
    }
    let raw = serde_json::from_str::<Value>(&response.body)
        .map_err(|error| AuthError::new(error.to_string()))?;
    sanitize_radius_gateway_config(&raw)
        .ok_or_else(|| AuthError::new(format!("Invalid Radius config from {gateway}")))
}

pub async fn load_radius_gateway_config(
    gateway: &str,
    api_key: Option<&str>,
    signal: Option<Arc<dyn AbortSignal>>,
) -> Result<RadiusGatewayConfig, AuthError> {
    load_radius_gateway_config_with(
        default_fetch(),
        gateway,
        api_key,
        crate::utils::abort::operation_signal(signal),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::oauth::test_support::{fetch, response};
    use crate::utils::abort::AbortController;
    use serde_json::json;
    use std::sync::Mutex;

    /// Pins pi `src/providers/radius-config.ts:26-50`'s intentionally shallow validation.
    #[test]
    fn sanitizer_filters_only_models_missing_the_runtime_checked_fields() {
        let config = sanitize_radius_gateway_config(&json!({
            "baseUrl": "https://api.radius.test",
            "models": [
                {
                    "id": "model",
                    "name": "Model",
                    "reasoning": true,
                    "thinkingLevelMap": { "high": "high" },
                    "input": ["audio"],
                    "cost": { "custom": 1 },
                    "contextWindow": 100,
                    "maxTokens": 20,
                    "unknown": "preserved"
                },
                { "id": "missing-fields" }
            ]
        }))
        .expect("config");
        assert_eq!(config.models.len(), 1);
        assert_eq!(config.models[0].input, [Value::String("audio".to_owned())]);
        assert_eq!(config.models[0].cost["custom"], 1);
        assert_eq!(config.models[0].extra["unknown"], "preserved");
    }

    /// Pins pi `src/providers/radius-config.ts:52-73`.
    #[test]
    fn normalizes_gateway_and_maps_runtime_models() {
        assert_eq!(
            normalize_radius_gateway_url("radius.test///"),
            "https://radius.test"
        );
        assert_eq!(
            normalize_radius_gateway_url("HTTP://radius.test/"),
            "HTTP://radius.test"
        );
        let credential = OAuthCredential {
            kind: crate::auth::OAuthCredentialType::OAuth,
            refresh: "r".to_owned(),
            access: "a".to_owned(),
            expires: 0.0,
            extra: Map::from_iter([(
                "gatewayConfig".to_owned(),
                json!({ "baseUrl": "https://api.test", "models": [] }),
            )]),
        };
        assert!(get_radius_models("radius", Some(&credential)).is_empty());
        assert_eq!(
            get_radius_credential_config(Some(&credential))
                .expect("config")
                .base_url,
            "https://api.test"
        );
    }

    /// Pins pi `src/providers/radius-config.ts:61-68`'s trailing spread overwrites.
    #[test]
    fn resolved_models_overwrite_reserved_gateway_fields_without_duplicate_keys() {
        let config = RadiusGatewayConfig {
            base_url: "https://resolved.example".to_owned(),
            models: vec![RadiusGatewayModel {
                id: "model".to_owned(),
                name: "Model".to_owned(),
                reasoning: false,
                thinking_level_map: None,
                input: vec![Value::String("text".to_owned())],
                cost: Map::new(),
                context_window: 1_000.0,
                max_tokens: 100.0,
                extra: Map::from_iter([
                    ("api".to_owned(), Value::String("gateway-api".to_owned())),
                    (
                        "provider".to_owned(),
                        Value::String("gateway-provider".to_owned()),
                    ),
                    (
                        "baseUrl".to_owned(),
                        Value::String("https://gateway.example".to_owned()),
                    ),
                    ("preserved".to_owned(), Value::Bool(true)),
                ]),
            }],
        };
        let resolved = get_radius_models_from_config("radius", &config);
        let serialized = serde_json::to_string(&resolved[0]).expect("resolved model");
        assert_eq!(serialized.matches("\"api\"").count(), 1);
        assert_eq!(serialized.matches("\"provider\"").count(), 1);
        assert_eq!(serialized.matches("\"baseUrl\"").count(), 1);
        let value = serde_json::from_str::<Value>(&serialized).expect("resolved model JSON");
        assert_eq!(value["api"], "pi-messages");
        assert_eq!(value["provider"], "radius");
        assert_eq!(value["baseUrl"], "https://resolved.example");
        assert_eq!(value["preserved"], true);
    }

    /// Pins pi `src/providers/radius-config.ts:75-96`.
    #[tokio::test]
    async fn loads_with_bearer_auth_and_truncates_error_bodies() {
        let captured = Arc::new(Mutex::new(None));
        let request_slot = captured.clone();
        let fetcher = fetch(move |request| {
            *request_slot.lock().expect("request") = Some(request);
            Ok(response(
                200,
                r#"{"baseUrl":"https://api.test","models":[]}"#,
            ))
        });
        let config = load_radius_gateway_config_with(
            fetcher,
            "https://radius.test",
            Some("key"),
            AbortController::new().signal(),
        )
        .await
        .expect("config");
        assert_eq!(config.base_url, "https://api.test");
        let request = captured.lock().expect("request").take().expect("request");
        assert_eq!(request.url, "https://radius.test/v1/config");
        assert_eq!(request.headers["authorization"], "Bearer key");
        assert_eq!(truncate_http_body(&"x".repeat(513)).chars().count(), 513);
        assert!(truncate_http_body(&"x".repeat(513)).ends_with('…'));
        assert_eq!(
            truncate_http_body(&"🦀".repeat(257)).encode_utf16().count(),
            513
        );
    }
}
