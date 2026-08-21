use super::device_code::{
    OAuthDeviceCodePollOptions, OAuthDeviceCodePollResult, abortable_sleep,
    poll_oauth_device_code_flow,
};
use super::{OAuthHttpError, form, request, send_http};
use crate::auth::types::{
    AuthError, AuthEvent, AuthFuture, AuthPrompt, ModelAuth, OAuthAuth, OAuthCredential,
    OAuthCredentialType, ProviderAuthInteraction,
};
use crate::providers::github_copilot_models::GITHUB_COPILOT_MODELS;
use crate::types::{FetchFunction, default_fetch};
use serde_json::{Map, Value, json};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

const CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";
const COPILOT_API_VERSION: &str = "2026-06-01";

#[derive(Clone)]
struct CopilotUrls {
    device_code_url: String,
    access_token_url: String,
    copilot_token_url: String,
}

#[derive(Debug)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    interval: Option<f64>,
    expires_in: f64,
}

#[derive(Debug, PartialEq, Eq)]
struct ModelCatalog {
    available_model_ids: Vec<String>,
    policy_model_ids: Vec<String>,
}

#[derive(Clone, Copy)]
struct RetryPolicy {
    max_retries: usize,
    max_elapsed: Duration,
}

fn copilot_headers() -> Vec<(String, String)> {
    vec![
        (
            "User-Agent".to_owned(),
            "GitHubCopilotChat/0.35.0".to_owned(),
        ),
        ("Editor-Version".to_owned(), "vscode/1.107.0".to_owned()),
        (
            "Editor-Plugin-Version".to_owned(),
            "copilot-chat/0.35.0".to_owned(),
        ),
        (
            "Copilot-Integration-Id".to_owned(),
            "vscode-chat".to_owned(),
        ),
    ]
}

fn normalize_domain(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    let raw = if trimmed.contains("://") {
        trimmed.to_owned()
    } else {
        format!("https://{trimmed}")
    };
    url::Url::parse(&raw)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
}

fn get_urls(domain: &str) -> CopilotUrls {
    CopilotUrls {
        device_code_url: format!("https://{domain}/login/device/code"),
        access_token_url: format!("https://{domain}/login/oauth/access_token"),
        copilot_token_url: format!("https://api.{domain}/copilot_internal/v2/token"),
    }
}

fn get_base_url_from_token(token: &str) -> Option<String> {
    let mut remaining = token;
    let proxy_host = loop {
        let start = remaining.find("proxy-ep=")? + "proxy-ep=".len();
        remaining = &remaining[start..];
        let host = remaining.split(';').next().unwrap_or_default();
        if !host.is_empty() {
            break host;
        }
    };
    let api_host = proxy_host
        .strip_prefix("proxy.")
        .map(|suffix| format!("api.{suffix}"))
        .unwrap_or_else(|| proxy_host.to_owned());
    Some(format!("https://{api_host}"))
}

fn get_github_copilot_base_url(token: Option<&str>, enterprise_domain: Option<&str>) -> String {
    if let Some(url) = token.and_then(get_base_url_from_token) {
        return url;
    }
    enterprise_domain
        .filter(|domain| !domain.is_empty())
        .map_or_else(
            || "https://api.individual.githubcopilot.com".to_owned(),
            |domain| format!("https://copilot-api.{domain}"),
        )
}

fn parse_github_copilot_model_catalog(
    raw: &Value,
    allow_policy_fallback: bool,
) -> Result<ModelCatalog, AuthError> {
    let data = raw
        .as_object()
        .and_then(|raw| raw.get("data"))
        .and_then(Value::as_array)
        .ok_or_else(|| AuthError::new("Invalid Copilot models response"))?;
    struct AccountModel<'a> {
        id: &'a str,
        picker_enabled: bool,
        policy_state: Option<&'a str>,
    }
    let account_models = data
        .iter()
        .filter_map(|item| {
            let item = item.as_object()?;
            let id = item.get("id")?.as_str()?;
            let tool_calls = item
                .get("capabilities")
                .and_then(Value::as_object)
                .and_then(|capabilities| capabilities.get("supports"))
                .and_then(Value::as_object)
                .and_then(|supports| supports.get("tool_calls"));
            if tool_calls == Some(&Value::Bool(false)) {
                return None;
            }
            Some(AccountModel {
                id,
                picker_enabled: item.get("model_picker_enabled") == Some(&Value::Bool(true)),
                policy_state: item
                    .get("policy")
                    .and_then(Value::as_object)
                    .and_then(|policy| policy.get("state"))
                    .and_then(Value::as_str),
            })
        })
        .collect::<Vec<_>>();
    let picker_model_ids = account_models
        .iter()
        .filter(|model| model.picker_enabled && model.policy_state != Some("disabled"))
        .map(|model| model.id.to_owned())
        .collect::<Vec<_>>();
    let use_policy_fallback = allow_policy_fallback && picker_model_ids.is_empty();
    let available_model_ids = if !picker_model_ids.is_empty() || !allow_policy_fallback {
        picker_model_ids
    } else {
        account_models
            .iter()
            .filter(|model| model.policy_state == Some("enabled"))
            .map(|model| model.id.to_owned())
            .collect()
    };
    let policy_model_ids = account_models
        .iter()
        .filter(|model| {
            model.policy_state == Some("unconfigured")
                && GITHUB_COPILOT_MODELS.contains_key(model.id)
                && (model.picker_enabled || use_policy_fallback)
        })
        .map(|model| model.id.to_owned())
        .collect();
    Ok(ModelCatalog {
        available_model_ids,
        policy_model_ids,
    })
}

async fn fetch_with_rate_limit_retry(
    fetch: Arc<dyn FetchFunction>,
    make_request: impl Fn() -> crate::types::ProviderHttpRequest,
    signal: Arc<dyn crate::types::AbortSignal>,
    retry_policy: RetryPolicy,
) -> Result<super::OAuthHttpResponse, AuthError> {
    let deadline = (retry_policy.max_retries > 0 && !retry_policy.max_elapsed.is_zero())
        .then(|| tokio::time::Instant::now() + retry_policy.max_elapsed);
    for retry in 0.. {
        let request_timeout = deadline
            .map(|deadline| deadline.saturating_duration_since(tokio::time::Instant::now()))
            .map(|remaining| remaining.min(Duration::from_secs(5)))
            .unwrap_or(Duration::from_secs(5));
        let response = send_http(fetch.clone(), make_request(), Some(request_timeout))
            .await
            .map_err(|error| match error {
                OAuthHttpError::Aborted => AuthError::new("Login cancelled"),
                other => AuthError::new(other.to_string()),
            })?;
        if response.status != 429 || retry == retry_policy.max_retries {
            return Ok(response);
        }

        let retry_after = response
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("retry-after"))
            .map(|(_, value)| value.as_str())
            .filter(|value| !value.is_empty());
        let mut delay_ms = 500.0 * 2_f64.powi(retry as i32);
        if let Some(retry_after) = retry_after {
            delay_ms = javascript_parse_float(retry_after).map_or_else(
                || {
                    httpdate::parse_http_date(retry_after).map_or(f64::NAN, |date| {
                        date.duration_since(SystemTime::now())
                            .map_or(0.0, |duration| duration.as_secs_f64() * 1_000.0)
                    })
                },
                |seconds| seconds * 1_000.0,
            );
            if !delay_ms.is_finite() {
                return Ok(response);
            }
        }
        delay_ms = delay_ms.max(0.0);
        if deadline.is_some_and(|deadline| {
            Duration::from_secs_f64(delay_ms / 1_000.0)
                >= deadline.saturating_duration_since(tokio::time::Instant::now())
        }) {
            return Ok(response);
        }
        abortable_sleep(delay_ms, signal.clone(), "Login cancelled").await?;
    }
    unreachable!()
}

fn javascript_parse_float(input: &str) -> Option<f64> {
    let input = input.trim_start();
    let bytes = input.as_bytes();
    let mut index = usize::from(matches!(bytes.first(), Some(b'+' | b'-')));
    if input[index..].starts_with("Infinity") {
        return Some(if bytes.first() == Some(&b'-') {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        });
    }

    let integer_start = index;
    while bytes.get(index).is_some_and(u8::is_ascii_digit) {
        index += 1;
    }
    let mut has_digits = index > integer_start;
    if bytes.get(index) == Some(&b'.') {
        index += 1;
        let fraction_start = index;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        has_digits |= index > fraction_start;
    }
    if !has_digits {
        return None;
    }

    if matches!(bytes.get(index), Some(b'e' | b'E')) {
        let exponent_marker = index;
        index += 1;
        if matches!(bytes.get(index), Some(b'+' | b'-')) {
            index += 1;
        }
        let exponent_start = index;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        if index == exponent_start {
            index = exponent_marker;
        }
    }

    input[..index].parse().ok()
}

async fn fetch_github_copilot_models(
    fetch: Arc<dyn FetchFunction>,
    copilot_token: &str,
    enterprise_domain: Option<&str>,
    signal: Arc<dyn crate::types::AbortSignal>,
    retry_policy: RetryPolicy,
) -> Result<ModelCatalog, AuthError> {
    let base_url = get_github_copilot_base_url(Some(copilot_token), enterprise_domain);
    let allow_policy_fallback = base_url == "https://api.individual.githubcopilot.com";
    let url = format!("{base_url}/models");
    let token = copilot_token.to_owned();
    let request_signal = signal.clone();
    let response = fetch_with_rate_limit_retry(
        fetch,
        move || {
            let mut headers = copilot_headers();
            headers.extend([
                ("Accept".to_owned(), "application/json".to_owned()),
                ("Authorization".to_owned(), format!("Bearer {token}")),
                (
                    "X-GitHub-Api-Version".to_owned(),
                    COPILOT_API_VERSION.to_owned(),
                ),
            ]);
            request(
                "GET",
                url.clone(),
                headers,
                Vec::new(),
                request_signal.clone(),
            )
        },
        signal,
        retry_policy,
    )
    .await?;
    if !response.ok() {
        return Err(AuthError::new(format!(
            "{} {}: {}",
            response.status, response.status_text, response.body
        )));
    }
    let raw =
        serde_json::from_str(&response.body).map_err(|error| AuthError::new(error.to_string()))?;
    parse_github_copilot_model_catalog(&raw, allow_policy_fallback)
}

async fn fetch_json(
    fetch: Arc<dyn FetchFunction>,
    request: crate::types::ProviderHttpRequest,
) -> Result<Value, AuthError> {
    let response = send_http(fetch, request, None)
        .await
        .map_err(|error| AuthError::new(error.to_string()))?;
    if !response.ok() {
        return Err(AuthError::new(format!(
            "{} {}: {}",
            response.status, response.status_text, response.body
        )));
    }
    serde_json::from_str(&response.body).map_err(|error| AuthError::new(error.to_string()))
}

async fn start_device_flow(
    fetch: Arc<dyn FetchFunction>,
    domain: &str,
    signal: Arc<dyn crate::types::AbortSignal>,
) -> Result<DeviceCodeResponse, AuthError> {
    let urls = get_urls(domain);
    let data = fetch_json(
        fetch,
        request(
            "POST",
            urls.device_code_url,
            [
                ("Accept", "application/json"),
                ("Content-Type", "application/x-www-form-urlencoded"),
                ("User-Agent", "GitHubCopilotChat/0.35.0"),
            ],
            form(&[("client_id", CLIENT_ID), ("scope", "read:user")]),
            signal,
        ),
    )
    .await?;
    let object = data
        .as_object()
        .ok_or_else(|| AuthError::new("Invalid device code response"))?;
    let device_code = object.get("device_code").and_then(Value::as_str);
    let user_code = object.get("user_code").and_then(Value::as_str);
    let verification_uri = object.get("verification_uri").and_then(Value::as_str);
    let interval = object.get("interval").and_then(Value::as_f64);
    let expires_in = object.get("expires_in").and_then(Value::as_f64);
    let (Some(device_code), Some(user_code), Some(verification_uri), Some(expires_in)) =
        (device_code, user_code, verification_uri, expires_in)
    else {
        return Err(AuthError::new("Invalid device code response fields"));
    };
    if object.contains_key("interval") && interval.is_none() {
        return Err(AuthError::new("Invalid device code response fields"));
    }
    let parsed_uri = url::Url::parse(verification_uri)
        .map_err(|_| AuthError::new("Untrusted verification_uri in device code response"))?;
    if !matches!(parsed_uri.scheme(), "http" | "https") {
        return Err(AuthError::new(
            "Untrusted verification_uri in device code response",
        ));
    }
    Ok(DeviceCodeResponse {
        device_code: device_code.to_owned(),
        user_code: user_code.to_owned(),
        verification_uri: parsed_uri.to_string(),
        interval,
        expires_in,
    })
}

async fn poll_for_github_access_token(
    fetch: Arc<dyn FetchFunction>,
    domain: String,
    device: DeviceCodeResponse,
    signal: Arc<dyn crate::types::AbortSignal>,
) -> Result<String, AuthError> {
    let urls = get_urls(&domain);
    let poll_signal = signal.clone();
    poll_oauth_device_code_flow(
        OAuthDeviceCodePollOptions {
            interval_seconds: device.interval,
            expires_in_seconds: Some(device.expires_in),
            wait_before_first_poll: true,
            signal,
        },
        move || {
            let fetch = fetch.clone();
            let url = urls.access_token_url.clone();
            let device_code = device.device_code.clone();
            let signal = poll_signal.clone();
            async move {
                let raw = fetch_json(
                    fetch,
                    request(
                        "POST",
                        url,
                        [
                            ("Accept", "application/json"),
                            ("Content-Type", "application/x-www-form-urlencoded"),
                            ("User-Agent", "GitHubCopilotChat/0.35.0"),
                        ],
                        form(&[
                            ("client_id", CLIENT_ID),
                            ("device_code", &device_code),
                            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                        ]),
                        signal,
                    ),
                )
                .await?;
                let Some(object) = raw.as_object() else {
                    return Ok(OAuthDeviceCodePollResult::Failed {
                        message: "Invalid device token response".to_owned(),
                    });
                };
                if let Some(access_token) = object.get("access_token").and_then(Value::as_str) {
                    return Ok(OAuthDeviceCodePollResult::Complete(access_token.to_owned()));
                }
                let Some(error) = object.get("error").and_then(Value::as_str) else {
                    return Ok(OAuthDeviceCodePollResult::Failed {
                        message: "Invalid device token response".to_owned(),
                    });
                };
                Ok(match error {
                    "authorization_pending" => OAuthDeviceCodePollResult::Pending,
                    "slow_down" => OAuthDeviceCodePollResult::SlowDown {
                        interval_seconds: object.get("interval").and_then(Value::as_f64),
                    },
                    _ => OAuthDeviceCodePollResult::Failed {
                        message: format!(
                            "Device flow failed: {error}{}",
                            object
                                .get("error_description")
                                .filter(|description| javascript_truthy(description))
                                .map(|description| format!(": {}", javascript_string(description)))
                                .unwrap_or_default()
                        ),
                    },
                })
            }
        },
    )
    .await
}

fn javascript_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_some_and(|value| value != 0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

fn javascript_string(value: &Value) -> String {
    match value {
        Value::Null => "null".to_owned(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value
            .as_f64()
            .filter(|value| value.fract() == 0.0)
            .map_or_else(|| value.to_string(), |value| format!("{value:.0}")),
        Value::String(value) => value.clone(),
        Value::Array(values) => values
            .iter()
            .map(|value| match value {
                Value::Null => String::new(),
                Value::Array(_) => javascript_string(value),
                Value::Object(_) => "[object Object]".to_owned(),
                _ => javascript_string(value),
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".to_owned(),
    }
}

async fn refresh_github_copilot_access_token(
    fetch: Arc<dyn FetchFunction>,
    refresh_token: String,
    enterprise_domain: Option<String>,
    signal: Arc<dyn crate::types::AbortSignal>,
) -> Result<OAuthCredential, AuthError> {
    let domain = enterprise_domain
        .as_deref()
        .filter(|domain| !domain.is_empty())
        .unwrap_or("github.com");
    let urls = get_urls(domain);
    let mut headers = copilot_headers();
    headers.extend([
        ("Accept".to_owned(), "application/json".to_owned()),
        (
            "Authorization".to_owned(),
            format!("Bearer {refresh_token}"),
        ),
    ]);
    let raw = fetch_json(
        fetch,
        request("GET", urls.copilot_token_url, headers, Vec::new(), signal),
    )
    .await?;
    let object = raw
        .as_object()
        .ok_or_else(|| AuthError::new("Invalid Copilot token response"))?;
    let token = object.get("token").and_then(Value::as_str);
    let expires_at = object.get("expires_at").and_then(Value::as_f64);
    let (Some(token), Some(expires_at)) = (token, expires_at) else {
        return Err(AuthError::new("Invalid Copilot token response fields"));
    };
    let mut extra = Map::new();
    if let Some(domain) = enterprise_domain {
        extra.insert("enterpriseUrl".to_owned(), Value::String(domain));
    }
    Ok(OAuthCredential {
        kind: OAuthCredentialType::OAuth,
        refresh: refresh_token,
        access: token.to_owned(),
        expires: expires_at * 1_000.0 - 5.0 * 60.0 * 1_000.0,
        extra,
    })
}

async fn refresh_github_copilot_token(
    fetch: Arc<dyn FetchFunction>,
    refresh_token: String,
    enterprise_domain: Option<String>,
    signal: Arc<dyn crate::types::AbortSignal>,
) -> Result<OAuthCredential, AuthError> {
    let mut credential = refresh_github_copilot_access_token(
        fetch.clone(),
        refresh_token,
        enterprise_domain.clone(),
        signal.clone(),
    )
    .await?;
    let models = fetch_github_copilot_models(
        fetch,
        &credential.access,
        enterprise_domain.as_deref(),
        signal,
        RetryPolicy {
            max_retries: 0,
            max_elapsed: Duration::ZERO,
        },
    )
    .await?;
    credential.extra.insert(
        "availableModelIds".to_owned(),
        Value::Array(
            models
                .available_model_ids
                .into_iter()
                .map(Value::String)
                .collect(),
        ),
    );
    Ok(credential)
}

async fn enable_github_copilot_model(
    fetch: Arc<dyn FetchFunction>,
    token: &str,
    model_id: &str,
    enterprise_domain: Option<&str>,
    signal: Arc<dyn crate::types::AbortSignal>,
) -> Result<bool, AuthError> {
    let base_url = get_github_copilot_base_url(Some(token), enterprise_domain);
    let url = format!("{base_url}/models/{model_id}/policy");
    let token = token.to_owned();
    let request_signal = signal.clone();
    let response = fetch_with_rate_limit_retry(
        fetch,
        move || {
            let mut headers = copilot_headers();
            headers.extend([
                ("Content-Type".to_owned(), "application/json".to_owned()),
                ("Authorization".to_owned(), format!("Bearer {token}")),
                ("openai-intent".to_owned(), "chat-policy".to_owned()),
                ("x-interaction-type".to_owned(), "chat-policy".to_owned()),
            ]);
            request(
                "POST",
                url.clone(),
                headers,
                serde_json::to_vec(&json!({ "state": "enabled" })).expect("static JSON"),
                request_signal.clone(),
            )
        },
        signal.clone(),
        RetryPolicy {
            max_retries: 2,
            max_elapsed: Duration::from_secs(5),
        },
    )
    .await;
    let response = match response {
        Ok(response) => response,
        Err(_error) if !signal.is_aborted() => return Ok(false),
        Err(error) => return Err(error),
    };
    if response.status == 429 {
        return Err(AuthError::new(format!(
            "{} {}: {}",
            response.status, response.status_text, response.body
        )));
    }
    Ok(response.ok())
}

async fn enable_github_copilot_models(
    fetch: Arc<dyn FetchFunction>,
    token: &str,
    model_ids: &[String],
    enterprise_domain: Option<&str>,
    signal: Arc<dyn crate::types::AbortSignal>,
) -> Result<Vec<String>, AuthError> {
    let mut enabled = Vec::new();
    for model_id in model_ids {
        match enable_github_copilot_model(
            fetch.clone(),
            token,
            model_id,
            enterprise_domain,
            signal.clone(),
        )
        .await
        {
            Ok(true) => enabled.push(model_id.clone()),
            Ok(false) => {}
            Err(error) if signal.is_aborted() => return Err(error),
            Err(_) => break,
        }
    }
    Ok(enabled)
}

async fn login_github_copilot(
    fetch: Arc<dyn FetchFunction>,
    interaction: ProviderAuthInteraction,
) -> Result<OAuthCredential, AuthError> {
    let input = interaction
        .interaction
        .prompt(AuthPrompt::Text {
            message: "GitHub Enterprise URL/domain (blank for github.com)".to_owned(),
            placeholder: Some("company.ghe.com".to_owned()),
            signal: None,
        })
        .await?;
    if interaction.signal.is_aborted() {
        return Err(AuthError::new("Login cancelled"));
    }
    let trimmed = input.trim();
    let enterprise_domain = normalize_domain(&input);
    if !trimmed.is_empty() && enterprise_domain.is_none() {
        return Err(AuthError::new("Invalid GitHub Enterprise URL/domain"));
    }
    let domain = enterprise_domain.as_deref().unwrap_or("github.com");
    let device = start_device_flow(fetch.clone(), domain, interaction.signal.clone()).await?;
    interaction.interaction.notify(AuthEvent::DeviceCode {
        user_code: device.user_code.clone(),
        verification_uri: device.verification_uri.clone(),
        interval_seconds: device.interval,
        expires_in_seconds: Some(device.expires_in),
    });
    let github_access_token = poll_for_github_access_token(
        fetch.clone(),
        domain.to_owned(),
        device,
        interaction.signal.clone(),
    )
    .await?;
    let mut credential = refresh_github_copilot_access_token(
        fetch.clone(),
        github_access_token,
        enterprise_domain.clone(),
        interaction.signal.clone(),
    )
    .await?;
    let models = fetch_github_copilot_models(
        fetch.clone(),
        &credential.access,
        enterprise_domain.as_deref(),
        interaction.signal.clone(),
        RetryPolicy {
            max_retries: 2,
            max_elapsed: Duration::from_secs(5),
        },
    )
    .await?;
    let mut available = models.available_model_ids;
    if !models.policy_model_ids.is_empty() {
        interaction.interaction.notify(AuthEvent::Progress {
            message: "Enabling models...".to_owned(),
        });
        available.extend(
            enable_github_copilot_models(
                fetch,
                &credential.access,
                &models.policy_model_ids,
                enterprise_domain.as_deref(),
                interaction.signal,
            )
            .await?,
        );
    }
    let mut deduplicated = Vec::new();
    for id in available {
        if !deduplicated.contains(&id) {
            deduplicated.push(id);
        }
    }
    credential.extra.insert(
        "availableModelIds".to_owned(),
        Value::Array(deduplicated.into_iter().map(Value::String).collect()),
    );
    Ok(credential)
}

fn copilot_enterprise_domain(credential: &OAuthCredential) -> Option<String> {
    credential
        .extra
        .get("enterpriseUrl")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .and_then(normalize_domain)
}

fn github_copilot_oauth_with(fetch: Arc<dyn FetchFunction>) -> OAuthAuth {
    let login_fetch = fetch.clone();
    let refresh_fetch = fetch;
    OAuthAuth {
        name: "GitHub Copilot".to_owned(),
        is_subscription: Some(true),
        login_label: None,
        login: Arc::new(move |interaction| {
            Box::pin(login_github_copilot(login_fetch.clone(), interaction))
                as AuthFuture<OAuthCredential>
        }),
        refresh: Arc::new(move |credential, signal| {
            let enterprise_domain = copilot_enterprise_domain(&credential);
            Box::pin(refresh_github_copilot_token(
                refresh_fetch.clone(),
                credential.refresh,
                enterprise_domain,
                signal,
            )) as AuthFuture<OAuthCredential>
        }),
        to_auth: Arc::new(|credential| {
            Box::pin(async move {
                let enterprise_domain = copilot_enterprise_domain(&credential);
                Ok(ModelAuth {
                    api_key: Some(credential.access.clone()),
                    base_url: Some(get_github_copilot_base_url(
                        Some(&credential.access),
                        enterprise_domain.as_deref(),
                    )),
                    ..ModelAuth::default()
                })
            })
        }),
    }
}

pub fn github_copilot_oauth() -> OAuthAuth {
    github_copilot_oauth_with(default_fetch())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::oauth::test_support::{fetch, response};
    use crate::auth::types::{AuthInteraction, AuthPrompt};
    use crate::types::ProviderHttpRequest;
    use crate::utils::abort::AbortController;
    use std::collections::{BTreeMap, VecDeque};
    use std::sync::Mutex;

    #[derive(Default)]
    struct LoginInteraction {
        events: Mutex<Vec<AuthEvent>>,
    }

    impl AuthInteraction for LoginInteraction {
        fn signal(&self) -> Option<Arc<dyn crate::types::AbortSignal>> {
            None
        }

        fn prompt(&self, prompt: AuthPrompt) -> AuthFuture<String> {
            let AuthPrompt::Text {
                message,
                placeholder,
                ..
            } = prompt
            else {
                panic!("expected enterprise-domain prompt")
            };
            assert_eq!(
                message,
                "GitHub Enterprise URL/domain (blank for github.com)"
            );
            assert_eq!(placeholder.as_deref(), Some("company.ghe.com"));
            Box::pin(async { Ok(String::new()) })
        }

        fn notify(&self, event: AuthEvent) {
            self.events.lock().expect("events").push(event);
        }
    }

    fn credential(refresh: &str, enterprise: Option<&str>) -> OAuthCredential {
        let mut extra = Map::new();
        if let Some(enterprise) = enterprise {
            extra.insert(
                "enterpriseUrl".to_owned(),
                Value::String(enterprise.to_owned()),
            );
        }
        OAuthCredential {
            kind: OAuthCredentialType::OAuth,
            refresh: refresh.to_owned(),
            access: "old".to_owned(),
            expires: 0.0,
            extra,
        }
    }

    /// Ports pi `test/oauth-auth.test.ts:58` and `:64`.
    #[tokio::test]
    async fn derives_proxy_enterprise_and_individual_base_urls() {
        let oauth = github_copilot_oauth();
        let mut proxy = credential("r", None);
        proxy.access = "tid=abc;proxy-ep=proxy.enterprise.example;rest".to_owned();
        assert_eq!(
            (oauth.to_auth)(proxy)
                .await
                .expect("auth")
                .base_url
                .as_deref(),
            Some("https://api.enterprise.example")
        );
        assert_eq!(
            (oauth.to_auth)(credential("r", Some("https://company.ghe.com")))
                .await
                .expect("auth")
                .base_url
                .as_deref(),
            Some("https://copilot-api.company.ghe.com")
        );
        assert_eq!(
            (oauth.to_auth)(credential("r", None))
                .await
                .expect("auth")
                .base_url
                .as_deref(),
            Some("https://api.individual.githubcopilot.com")
        );
        let mut repeated = credential("r", None);
        repeated.access =
            "tid=abc;proxy-ep=;other=value;proxy-ep=proxy.business.githubcopilot.com;".to_owned();
        assert_eq!(
            (oauth.to_auth)(repeated)
                .await
                .expect("auth")
                .base_url
                .as_deref(),
            Some("https://api.business.githubcopilot.com")
        );
    }

    /// Ports pi `test/github-copilot-oauth.test.ts:249` and `:549`.
    #[tokio::test(start_paused = true)]
    async fn login_emits_device_code_and_sends_the_device_and_token_request_shapes() {
        let replies = Arc::new(Mutex::new(VecDeque::from([
            response(
                200,
                r#"{"device_code":"device-code","user_code":"ABCD-EFGH","verification_uri":"https://github.com/login/device","interval":1,"expires_in":900}"#,
            ),
            response(200, r#"{"access_token":"ghu_refresh_token"}"#),
            response(
                200,
                r#"{"token":"tid=test;exp=9999999999;proxy-ep=proxy.individual.githubcopilot.com;","expires_at":9999999999}"#,
            ),
            response(200, r#"{"data":[]}"#),
        ])));
        let requests = Arc::new(Mutex::new(Vec::<ProviderHttpRequest>::new()));
        let fetcher = {
            let replies = replies.clone();
            let requests = requests.clone();
            fetch(move |request| {
                requests.lock().expect("requests").push(request);
                Ok(replies.lock().expect("replies").pop_front().expect("reply"))
            })
        };
        let interaction = Arc::new(LoginInteraction::default());
        let login = (github_copilot_oauth_with(fetcher).login)(
            crate::auth::helpers::normalize_interaction(interaction.clone()),
        );
        let task = tokio::spawn(login);
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(1)).await;
        let credential = task.await.expect("task").expect("credential");
        assert_eq!(credential.refresh, "ghu_refresh_token");
        assert!(matches!(
            interaction.events.lock().expect("events").as_slice(),
            [AuthEvent::DeviceCode {
                user_code,
                verification_uri,
                interval_seconds: Some(1.0),
                expires_in_seconds: Some(900.0),
            }] if user_code == "ABCD-EFGH" && verification_uri == "https://github.com/login/device"
        ));

        let requests = requests.lock().expect("requests");
        assert_eq!(
            requests
                .iter()
                .map(|request| request.url.as_str())
                .collect::<Vec<_>>(),
            [
                "https://github.com/login/device/code",
                "https://github.com/login/oauth/access_token",
                "https://api.github.com/copilot_internal/v2/token",
                "https://api.individual.githubcopilot.com/models",
            ]
        );
        let device = url::form_urlencoded::parse(requests[0].body.as_deref().expect("device body"))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(
            device.get("client_id").map(|value| value.as_ref()),
            Some(CLIENT_ID)
        );
        assert_eq!(
            device.get("scope").map(|value| value.as_ref()),
            Some("read:user")
        );
        let token = url::form_urlencoded::parse(requests[1].body.as_deref().expect("token body"))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(
            token.get("client_id").map(|value| value.as_ref()),
            Some(CLIENT_ID)
        );
        assert_eq!(
            token.get("device_code").map(|value| value.as_ref()),
            Some("device-code")
        );
        assert_eq!(
            token.get("grant_type").map(|value| value.as_ref()),
            Some("urn:ietf:params:oauth:grant-type:device_code")
        );
        assert_eq!(
            requests[2].headers.get("Authorization").map(String::as_str),
            Some("Bearer ghu_refresh_token")
        );
        assert_eq!(
            requests[3].headers.get("Authorization").map(String::as_str),
            Some("Bearer tid=test;exp=9999999999;proxy-ep=proxy.individual.githubcopilot.com;")
        );
    }

    /// Ports pi `test/github-copilot-oauth.test.ts:137`, `:166`, and `:202`.
    #[test]
    fn filters_picker_catalog_and_uses_individual_policy_fallback() {
        let raw = json!({ "data": [
            { "id": "gpt-4.1", "model_picker_enabled": true, "capabilities": { "supports": { "tool_calls": true } } },
            { "id": "claude-opus-4.7", "model_picker_enabled": true, "policy": { "state": "disabled" } },
            { "id": "gpt-4o", "model_picker_enabled": true, "capabilities": { "supports": { "tool_calls": false } } }
        ] });
        assert_eq!(
            parse_github_copilot_model_catalog(&raw, true)
                .expect("catalog")
                .available_model_ids,
            ["gpt-4.1"]
        );
        let fallback = json!({ "data": [
            { "id": "gpt-4.1", "model_picker_enabled": false, "policy": { "state": "enabled" } }
        ] });
        assert_eq!(
            parse_github_copilot_model_catalog(&fallback, true)
                .expect("catalog")
                .available_model_ids,
            ["gpt-4.1"]
        );
        assert!(
            parse_github_copilot_model_catalog(&fallback, false)
                .expect("catalog")
                .available_model_ids
                .is_empty()
        );
    }

    /// Ports pi `test/github-copilot-oauth.test.ts:308`.
    #[test]
    fn policy_updates_are_known_tool_capable_and_unconfigured() {
        let raw = json!({ "data": [
            { "id": "gpt-4.1", "model_picker_enabled": true, "policy": { "state": "unconfigured" } },
            { "id": "remote-only", "model_picker_enabled": true, "policy": { "state": "unconfigured" } },
            { "id": "gpt-5.4", "model_picker_enabled": true, "policy": { "state": "unconfigured" }, "capabilities": { "supports": { "tool_calls": false } } }
        ] });
        assert_eq!(
            parse_github_copilot_model_catalog(&raw, true)
                .expect("catalog")
                .policy_model_ids,
            ["gpt-4.1"]
        );
    }

    /// Ports pi `test/github-copilot-oauth.test.ts:218` and `:362`.
    #[tokio::test(start_paused = true)]
    async fn refresh_does_not_retry_catalog_429_while_policy_retry_honors_retry_after() {
        let replies = Arc::new(Mutex::new(VecDeque::from([
            response(
                200,
                r#"{"token":"tid=x;proxy-ep=proxy.individual.githubcopilot.com;","expires_at":9999999999}"#,
            ),
            response(429, r#"{"error":"rate"}"#),
        ])));
        let requests = Arc::new(Mutex::new(0_u32));
        let fetcher = {
            let replies = replies.clone();
            let requests = requests.clone();
            fetch(move |_| {
                *requests.lock().expect("requests") += 1;
                Ok(replies.lock().expect("replies").pop_front().expect("reply"))
            })
        };
        let error = (github_copilot_oauth_with(fetcher).refresh)(
            credential("ghu", None),
            AbortController::new().signal(),
        )
        .await
        .expect_err("429");
        assert!(error.message.starts_with("429"));
        assert_eq!(*requests.lock().expect("requests"), 2);

        let policy_replies = Arc::new(Mutex::new(VecDeque::from([
            {
                let mut headers = BTreeMap::new();
                headers.insert("retry-after".to_owned(), "1".to_owned());
                crate::auth::oauth::test_support::response_with_headers(
                    429,
                    r#"{"error":"rate"}"#,
                    headers,
                )
            },
            response(200, ""),
        ])));
        let fetcher = {
            let replies = policy_replies.clone();
            fetch(move |_| Ok(replies.lock().expect("replies").pop_front().expect("reply")))
        };
        let task = tokio::spawn(enable_github_copilot_model(
            fetcher,
            "tid=x;proxy-ep=proxy.individual.githubcopilot.com;",
            "gpt-4.1",
            None,
            AbortController::new().signal(),
        ));
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(task.await.expect("task").expect("policy"));
    }

    /// Ports pi `test/github-copilot-oauth.test.ts:393`.
    #[tokio::test]
    async fn policy_batch_continues_after_a_transport_failure() {
        let requests = Arc::new(Mutex::new(0_u32));
        let request_count = requests.clone();
        let fetcher = fetch(move |_| {
            let mut count = request_count.lock().expect("requests");
            *count += 1;
            if *count == 1 {
                Err("transport failed".to_owned())
            } else {
                Ok(response(200, ""))
            }
        });
        let models = ["gpt-4.1".to_owned(), "gpt-5".to_owned()];
        assert_eq!(
            enable_github_copilot_models(
                fetcher,
                "token",
                &models,
                None,
                AbortController::new().signal(),
            )
            .await
            .expect("batch"),
            ["gpt-5"]
        );
        assert_eq!(*requests.lock().expect("requests"), 2);
    }

    /// Ports pi `test/github-copilot-oauth.test.ts:420`.
    #[tokio::test]
    async fn exhausted_policy_retry_budget_stops_the_remaining_batch() {
        let requests = Arc::new(Mutex::new(0_u32));
        let request_count = requests.clone();
        let fetcher = fetch(move |_| {
            let mut count = request_count.lock().expect("requests");
            *count += 1;
            if *count == 1 {
                return Ok(response(200, ""));
            }
            let mut headers = BTreeMap::new();
            headers.insert("retry-after".to_owned(), "10".to_owned());
            Ok(crate::auth::oauth::test_support::response_with_headers(
                429,
                r#"{"error":"rate"}"#,
                headers,
            ))
        });
        let models = [
            "gpt-4.1".to_owned(),
            "gpt-5".to_owned(),
            "claude-sonnet-4".to_owned(),
        ];
        assert_eq!(
            enable_github_copilot_models(
                fetcher,
                "token",
                &models,
                None,
                AbortController::new().signal(),
            )
            .await
            .expect("batch"),
            ["gpt-4.1"]
        );
        assert_eq!(*requests.lock().expect("requests"), 2);
    }

    /// Ports pi `test/github-copilot-oauth.test.ts:454`.
    #[tokio::test]
    async fn rejects_non_http_verification_uri_before_notification() {
        let fetcher = fetch(|_| {
            Ok(response(
                200,
                r#"{"device_code":"d","user_code":"u","verification_uri":"file:///tmp/pwned","interval":1,"expires_in":9}"#,
            ))
        });
        let error = start_device_flow(fetcher, "github.com", AbortController::new().signal())
            .await
            .expect_err("untrusted");
        assert!(error.message.contains("Untrusted verification_uri"));
    }

    /// Ports pi `test/oauth-auth.test.ts:101` and
    /// `test/github-copilot-oauth.test.ts:137`'s refresh path.
    #[tokio::test]
    async fn refresh_preserves_enterprise_domain_and_stores_available_model_ids() {
        let urls = Arc::new(Mutex::new(Vec::new()));
        let captured_urls = urls.clone();
        let fetcher = fetch(move |request| {
            captured_urls
                .lock()
                .expect("urls")
                .push(request.url.clone());
            if request.url.ends_with("/models") {
                Ok(response(
                    200,
                    r#"{"data":[{"id":"gpt-4.1","model_picker_enabled":true}]}"#,
                ))
            } else {
                Ok(response(
                    200,
                    r#"{"token":"new-token","expires_at":9999999999}"#,
                ))
            }
        });
        let refreshed = (github_copilot_oauth_with(fetcher).refresh)(
            credential("gh-token", Some("company.ghe.com")),
            AbortController::new().signal(),
        )
        .await
        .expect("refresh");
        assert_eq!(refreshed.extra["enterpriseUrl"], "company.ghe.com");
        assert_eq!(refreshed.extra["availableModelIds"], json!(["gpt-4.1"]));
        let urls = urls.lock().expect("urls");
        assert_eq!(
            urls.as_slice(),
            [
                "https://api.company.ghe.com/copilot_internal/v2/token",
                "https://copilot-api.company.ghe.com/models"
            ]
        );
    }

    /// Ports pi `test/github-copilot-oauth.test.ts:484`.
    #[tokio::test]
    async fn normalizes_http_verification_uri_before_returning_device_info() {
        let raw = "https://github.com/login/\u{1b}]8;;evil";
        let fetcher = fetch(move |_| {
            Ok(response(
                200,
                json!({
                    "device_code": "d",
                    "user_code": "u",
                    "verification_uri": raw,
                    "interval": 1,
                    "expires_in": 9
                })
                .to_string(),
            ))
        });
        let device = start_device_flow(fetcher, "github.com", AbortController::new().signal())
            .await
            .expect("device");
        assert_ne!(device.verification_uri, raw);
        assert_eq!(
            device.verification_uri,
            url::Url::parse(raw).expect("URL").to_string()
        );
    }

    /// Pins pi `src/auth/oauth/github-copilot.ts:157`'s `Number.parseFloat` semantics.
    #[test]
    fn retry_after_parsing_accepts_a_numeric_prefix() {
        assert_eq!(javascript_parse_float("  10abc"), Some(10.0));
        assert_eq!(javascript_parse_float("-.5 seconds"), Some(-0.5));
        assert_eq!(javascript_parse_float("1e2ms"), Some(100.0));
        assert_eq!(javascript_parse_float("1e+oops"), Some(1.0));
        assert_eq!(
            javascript_parse_float("Wed, 21 Oct 2015 07:28:00 GMT"),
            None
        );
    }

    /// Pins pi `src/auth/oauth/github-copilot.ts:294-305`.
    #[test]
    fn device_flow_failure_description_uses_javascript_truthiness_and_string_coercion() {
        for (description, expected_suffix) in [
            (Value::String(String::new()), ""),
            (Value::Null, ""),
            (Value::Bool(false), ""),
            (json!(7), ": 7"),
            (json!({ "message": "detail" }), ": [object Object]"),
        ] {
            let suffix = if javascript_truthy(&description) {
                format!(": {}", javascript_string(&description))
            } else {
                String::new()
            };
            assert_eq!(suffix, expected_suffix);
        }
    }

    /// Pins pi `src/auth/oauth/github-copilot.ts:154-164`: an empty header is falsy.
    #[tokio::test(start_paused = true)]
    async fn empty_retry_after_uses_the_exponential_fallback() {
        let replies = Arc::new(Mutex::new(VecDeque::from([
            {
                let mut headers = BTreeMap::new();
                headers.insert("retry-after".to_owned(), String::new());
                crate::auth::oauth::test_support::response_with_headers(429, "", headers)
            },
            response(200, "ok"),
        ])));
        let calls = Arc::new(Mutex::new(0_u32));
        let fetcher = {
            let replies = replies.clone();
            let calls = calls.clone();
            fetch(move |_| {
                *calls.lock().expect("calls") += 1;
                Ok(replies.lock().expect("replies").pop_front().expect("reply"))
            })
        };
        let signal = AbortController::new().signal();
        let task = tokio::spawn(fetch_with_rate_limit_retry(
            fetcher,
            {
                let signal = signal.clone();
                move || {
                    request(
                        "GET",
                        "https://example.test".to_owned(),
                        std::iter::empty::<(&str, &str)>(),
                        Vec::new(),
                        signal.clone(),
                    )
                }
            },
            signal,
            RetryPolicy {
                max_retries: 1,
                max_elapsed: Duration::from_secs(5),
            },
        ));
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(499)).await;
        assert_eq!(*calls.lock().expect("calls"), 1);
        tokio::time::advance(Duration::from_millis(1)).await;
        assert_eq!(task.await.expect("task").expect("response").status, 200);
        assert_eq!(*calls.lock().expect("calls"), 2);
    }
}
