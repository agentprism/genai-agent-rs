use super::device_code::{
    OAuthDeviceCodePollOptions, OAuthDeviceCodePollResult, poll_oauth_device_code_flow,
};
use super::{form, now_millis, request, send_http};
use crate::auth::types::{
    AuthError, AuthEvent, AuthFuture, ModelAuth, OAuthAuth, OAuthCredential, OAuthCredentialType,
    ProviderAuthInteraction,
};
use crate::types::{FetchFunction, ProviderHeaders, default_fetch};
use crate::utils::provider_env::get_provider_env_value;
use crate::utils::sleep::sleep;
use serde_json::{Map, Value};
use std::sync::Arc;
use std::time::Duration;

const CLIENT_ID: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";
const DEFAULT_OAUTH_HOST: &str = "https://auth.kimi.com";
const DEVICE_CODE_TIMEOUT_SECONDS: f64 = 15.0 * 60.0;
const DEFAULT_POLL_INTERVAL_SECONDS: f64 = 5.0;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const REFRESH_MAX_RETRIES: usize = 3;

struct DeviceAuthorization {
    device_code: String,
    user_code: String,
    verification_uri_complete: String,
    interval_seconds: f64,
    expires_in_seconds: f64,
}

struct TokenResponse {
    access: String,
    refresh: String,
    expires: f64,
}

fn get_oauth_host_from(mut lookup: impl FnMut(&str) -> Option<String>) -> String {
    let host = lookup("KIMI_CODE_OAUTH_HOST")
        .filter(|host| !host.is_empty())
        .or_else(|| lookup("KIMI_OAUTH_HOST"))
        .filter(|host| !host.is_empty())
        .unwrap_or_else(|| DEFAULT_OAUTH_HOST.to_owned());
    host.trim_end_matches('/').to_owned()
}

fn get_oauth_host() -> String {
    get_oauth_host_from(|name| get_provider_env_value(name, None))
}

fn read_json(body: &str) -> Option<Value> {
    serde_json::from_str::<Value>(body)
        .ok()
        .filter(|value| value.is_object() || value.is_array())
}

fn trusted_http_url(value: &str) -> bool {
    !value.is_empty()
        && url::Url::parse(value).is_ok_and(|url| matches!(url.scheme(), "http" | "https"))
}

async fn start_device_authorization(
    fetch: Arc<dyn FetchFunction>,
    oauth_host: &str,
    signal: Arc<dyn crate::types::AbortSignal>,
) -> Result<DeviceAuthorization, AuthError> {
    let response = send_http(
        fetch,
        request(
            "POST",
            format!("{oauth_host}/api/oauth/device_authorization"),
            [
                ("Content-Type", "application/x-www-form-urlencoded"),
                ("Accept", "application/json"),
            ],
            form(&[("client_id", CLIENT_ID)]),
            signal,
        ),
        Some(REQUEST_TIMEOUT),
    )
    .await
    .map_err(|error| AuthError::new(error.to_string()))?;
    if !response.ok() {
        return Err(AuthError::new(format!(
            "Kimi Code device authorization failed with status {}{}",
            response.status,
            if response.body.is_empty() {
                String::new()
            } else {
                format!(": {}", response.body)
            }
        )));
    }

    let json = read_json(&response.body);
    let device_code = json
        .as_ref()
        .and_then(|json| json.get("device_code"))
        .and_then(Value::as_str);
    let user_code = json
        .as_ref()
        .and_then(|json| json.get("user_code"))
        .and_then(Value::as_str);
    let verification_uri = json
        .as_ref()
        .and_then(|json| json.get("verification_uri"))
        .and_then(Value::as_str);
    let verification_uri_complete = json
        .as_ref()
        .and_then(|json| json.get("verification_uri_complete"))
        .and_then(Value::as_str);
    let (
        Some(device_code),
        Some(user_code),
        Some(verification_uri),
        Some(verification_uri_complete),
    ) = (
        device_code,
        user_code,
        verification_uri,
        verification_uri_complete,
    )
    else {
        let rendered = json.unwrap_or(Value::Null).to_string();
        return Err(AuthError::new(format!(
            "Invalid Kimi Code device authorization response: {rendered}"
        )));
    };
    if !trusted_http_url(verification_uri) || !trusted_http_url(verification_uri_complete) {
        let rendered = json.unwrap_or(Value::Null).to_string();
        return Err(AuthError::new(format!(
            "Invalid Kimi Code device authorization response: {rendered}"
        )));
    }
    let interval_seconds = json
        .as_ref()
        .and_then(|json| json.get("interval"))
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite() && *value > 0.0)
        .unwrap_or(DEFAULT_POLL_INTERVAL_SECONDS);
    let expires_in_seconds = json
        .as_ref()
        .and_then(|json| json.get("expires_in"))
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite() && *value > 0.0)
        .unwrap_or(DEVICE_CODE_TIMEOUT_SECONDS);
    Ok(DeviceAuthorization {
        device_code: device_code.to_owned(),
        user_code: user_code.to_owned(),
        verification_uri_complete: verification_uri_complete.to_owned(),
        interval_seconds,
        expires_in_seconds,
    })
}

fn parse_token_response(json: Option<&Value>, operation: &str) -> Result<TokenResponse, AuthError> {
    let access = json
        .and_then(|json| json.get("access_token"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty());
    let refresh = json
        .and_then(|json| json.get("refresh_token"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty());
    let expires_in = json
        .and_then(|json| json.get("expires_in"))
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite() && *value > 0.0);
    let (Some(access), Some(refresh), Some(expires_in)) = (access, refresh, expires_in) else {
        let rendered = json.cloned().unwrap_or(Value::Null).to_string();
        return Err(AuthError::new(format!(
            "Kimi Code token {operation} response missing fields: {rendered}"
        )));
    };
    Ok(TokenResponse {
        access: access.to_owned(),
        refresh: refresh.to_owned(),
        expires: now_millis() + expires_in * 1_000.0,
    })
}

async fn poll_for_token(
    fetch: Arc<dyn FetchFunction>,
    oauth_host: String,
    device: DeviceAuthorization,
    signal: Arc<dyn crate::types::AbortSignal>,
) -> Result<TokenResponse, AuthError> {
    let poll_signal = signal.clone();
    poll_oauth_device_code_flow(
        OAuthDeviceCodePollOptions {
            interval_seconds: Some(device.interval_seconds),
            expires_in_seconds: Some(device.expires_in_seconds),
            wait_before_first_poll: true,
            signal,
        },
        move || {
            let fetch = fetch.clone();
            let oauth_host = oauth_host.clone();
            let device_code = device.device_code.clone();
            let signal = poll_signal.clone();
            async move {
                let response = send_http(
                    fetch,
                    request(
                        "POST",
                        format!("{oauth_host}/api/oauth/token"),
                        [
                            ("Content-Type", "application/x-www-form-urlencoded"),
                            ("Accept", "application/json"),
                        ],
                        form(&[
                            ("client_id", CLIENT_ID),
                            ("device_code", &device_code),
                            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                        ]),
                        signal,
                    ),
                    Some(REQUEST_TIMEOUT),
                )
                .await
                .map_err(|error| AuthError::new(error.to_string()))?;
                if response.status >= 500 {
                    return Ok(OAuthDeviceCodePollResult::Failed {
                        message: format!(
                            "Kimi Code device token request failed with status {}{}",
                            response.status,
                            if response.body.is_empty() {
                                String::new()
                            } else {
                                format!(": {}", response.body)
                            }
                        ),
                    });
                }
                let json = read_json(&response.body);
                if response.ok()
                    && json
                        .as_ref()
                        .and_then(|json| json.get("access_token"))
                        .is_some_and(Value::is_string)
                {
                    return Ok(match parse_token_response(json.as_ref(), "poll") {
                        Ok(token) => OAuthDeviceCodePollResult::Complete(token),
                        Err(error) => OAuthDeviceCodePollResult::Failed {
                            message: error.message,
                        },
                    });
                }
                let error = json
                    .as_ref()
                    .and_then(|json| json.get("error"))
                    .and_then(Value::as_str);
                let description = json
                    .as_ref()
                    .and_then(|json| json.get("error_description"))
                    .and_then(Value::as_str)
                    .map(|description| format!(": {description}"))
                    .unwrap_or_default();
                Ok(match error {
                    Some("authorization_pending") => OAuthDeviceCodePollResult::Pending,
                    Some("slow_down") => OAuthDeviceCodePollResult::SlowDown {
                        interval_seconds: json
                            .as_ref()
                            .and_then(|json| json.get("interval"))
                            .and_then(Value::as_f64)
                            .filter(|value| *value > 0.0),
                    },
                    Some("expired_token") => OAuthDeviceCodePollResult::Failed {
                        message: "Kimi Code device authorization expired. Please restart login."
                            .to_owned(),
                    },
                    Some("access_denied") => OAuthDeviceCodePollResult::Failed {
                        message: "Kimi Code login was denied.".to_owned(),
                    },
                    _ => OAuthDeviceCodePollResult::Failed {
                        message: format!(
                            "Kimi Code device token request failed (status {}){}",
                            response.status,
                            error
                                .map(|error| format!(": {error}{description}"))
                                .unwrap_or_default()
                        ),
                    },
                })
            }
        },
    )
    .await
}

fn retryable_refresh_failure(status: u16) -> bool {
    status == 429 || status >= 500
}

async fn refresh_token(
    fetch: Arc<dyn FetchFunction>,
    oauth_host: String,
    refresh_token_value: String,
    signal: Arc<dyn crate::types::AbortSignal>,
) -> Result<TokenResponse, AuthError> {
    let mut last_error = None;
    for attempt in 0..=REFRESH_MAX_RETRIES {
        if attempt > 0 {
            sleep(1_000.0 * 2_f64.powi(attempt as i32 - 1), signal.clone())
                .await
                .map_err(AuthError::abort)?;
        }
        if signal.is_aborted() {
            return Err(AuthError::new("Kimi Code token refresh aborted"));
        }
        let response = send_http(
            fetch.clone(),
            request(
                "POST",
                format!("{oauth_host}/api/oauth/token"),
                [
                    ("Content-Type", "application/x-www-form-urlencoded"),
                    ("Accept", "application/json"),
                ],
                form(&[
                    ("client_id", CLIENT_ID),
                    ("grant_type", "refresh_token"),
                    ("refresh_token", &refresh_token_value),
                ]),
                signal.clone(),
            ),
            Some(REQUEST_TIMEOUT),
        )
        .await;
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                last_error = Some(AuthError::new(error.to_string()));
                continue;
            }
        };
        let json = read_json(&response.body);
        if response.ok() {
            return parse_token_response(json.as_ref(), "refresh");
        }
        let oauth_error = json
            .as_ref()
            .and_then(|json| json.get("error"))
            .and_then(Value::as_str);
        if response.status == 401 || response.status == 403 || oauth_error == Some("invalid_grant")
        {
            let description = json
                .as_ref()
                .and_then(|json| json.get("error_description"))
                .and_then(Value::as_str)
                .map(|description| format!(": {description}"))
                .unwrap_or_default();
            return Err(AuthError::new(format!(
                "Kimi Code token refresh unauthorized (status {}){description}",
                response.status
            )));
        }
        if retryable_refresh_failure(response.status) && attempt < REFRESH_MAX_RETRIES {
            last_error = Some(AuthError::new(format!(
                "Kimi Code token refresh failed with status {}",
                response.status
            )));
            continue;
        }
        let rendered = json.unwrap_or(Value::Null).to_string();
        return Err(AuthError::new(format!(
            "Kimi Code token refresh failed with status {}{}",
            response.status,
            if rendered.is_empty() {
                String::new()
            } else {
                format!(": {rendered}")
            }
        )));
    }
    Err(last_error.unwrap_or_else(|| AuthError::new("Kimi Code token refresh failed")))
}

async fn login_kimi_coding(
    fetch: Arc<dyn FetchFunction>,
    host_override: Option<String>,
    interaction: ProviderAuthInteraction,
) -> Result<OAuthCredential, AuthError> {
    let oauth_host = host_override.unwrap_or_else(get_oauth_host);
    let device =
        start_device_authorization(fetch.clone(), &oauth_host, interaction.signal.clone()).await?;
    interaction.interaction.notify(AuthEvent::DeviceCode {
        user_code: device.user_code.clone(),
        verification_uri: device.verification_uri_complete.clone(),
        interval_seconds: Some(device.interval_seconds),
        expires_in_seconds: Some(device.expires_in_seconds),
    });
    let token = poll_for_token(fetch, oauth_host, device, interaction.signal).await?;
    Ok(OAuthCredential {
        kind: OAuthCredentialType::OAuth,
        access: token.access,
        refresh: token.refresh,
        expires: token.expires,
        extra: Map::new(),
    })
}

fn kimi_coding_oauth_with(
    fetch: Arc<dyn FetchFunction>,
    host_override: Option<String>,
) -> OAuthAuth {
    let login_fetch = fetch.clone();
    let login_host = host_override.clone();
    OAuthAuth {
        name: "Kimi Code (subscription)".to_owned(),
        is_subscription: Some(true),
        login_label: Some("Sign in with Kimi Code".to_owned()),
        login: Arc::new(move |interaction| {
            Box::pin(login_kimi_coding(
                login_fetch.clone(),
                login_host.clone(),
                interaction,
            )) as AuthFuture<OAuthCredential>
        }),
        refresh: Arc::new(move |credential, signal| {
            let fetch = fetch.clone();
            let oauth_host = host_override.clone().unwrap_or_else(get_oauth_host);
            Box::pin(async move {
                let token = refresh_token(fetch, oauth_host, credential.refresh, signal).await?;
                Ok(OAuthCredential {
                    kind: OAuthCredentialType::OAuth,
                    access: token.access,
                    refresh: token.refresh,
                    expires: token.expires,
                    extra: Map::new(),
                })
            }) as AuthFuture<OAuthCredential>
        }),
        to_auth: Arc::new(|credential| {
            Box::pin(async move {
                Ok(ModelAuth {
                    headers: Some(ProviderHeaders::from([(
                        "Authorization".to_owned(),
                        Some(format!("Bearer {}", credential.access)),
                    )])),
                    ..ModelAuth::default()
                })
            })
        }),
    }
}

pub fn kimi_coding_oauth() -> OAuthAuth {
    kimi_coding_oauth_with(default_fetch(), None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::oauth::test_support::{fetch, response};
    use crate::auth::types::{AuthInteraction, AuthPrompt};
    use crate::utils::abort::AbortController;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Interaction {
        events: Mutex<Vec<AuthEvent>>,
    }

    impl AuthInteraction for Interaction {
        fn signal(&self) -> Option<Arc<dyn crate::types::AbortSignal>> {
            None
        }

        fn prompt(&self, _prompt: AuthPrompt) -> AuthFuture<String> {
            Box::pin(async { Err(AuthError::new("Kimi Code login should not prompt")) })
        }

        fn notify(&self, event: AuthEvent) {
            self.events.lock().expect("events").push(event);
        }
    }

    fn device() -> crate::types::ProviderHttpResponse {
        response(
            200,
            r#"{"user_code":"ABCD-1234","device_code":"device-code-123","verification_uri":"https://www.kimi.com/code","verification_uri_complete":"https://www.kimi.com/code?user_code=ABCD-1234","interval":5,"expires_in":600}"#,
        )
    }

    /// Ports pi `test/kimi-coding-oauth.test.ts:52`.
    #[tokio::test(start_paused = true)]
    async fn logs_in_with_device_authorization_flow() {
        let replies = Arc::new(Mutex::new(VecDeque::from([
            device(),
            response(400, r#"{"error":"authorization_pending"}"#),
            response(
                200,
                r#"{"access_token":"access-token","refresh_token":"refresh-token","expires_in":3600}"#,
            ),
        ])));
        let fetch = {
            let replies = replies.clone();
            fetch(move |_| Ok(replies.lock().expect("replies").pop_front().expect("reply")))
        };
        let interaction = Arc::new(Interaction::default());
        let login = (kimi_coding_oauth_with(fetch, Some(DEFAULT_OAUTH_HOST.to_owned())).login)(
            crate::auth::helpers::normalize_interaction(interaction.clone()),
        );
        let task = tokio::spawn(login);
        tokio::time::advance(Duration::from_secs(10)).await;
        let credential = task.await.expect("task").expect("credential");
        assert_eq!(credential.access, "access-token");
        assert_eq!(credential.refresh, "refresh-token");
        assert!(matches!(
            interaction.events.lock().expect("events").as_slice(),
            [AuthEvent::DeviceCode { user_code, verification_uri, .. }]
                if user_code == "ABCD-1234" && verification_uri.contains("user_code=ABCD-1234")
        ));
    }

    /// Pins pi `src/auth/oauth/kimi-coding.ts:89-105`'s validate-without-normalizing behavior.
    #[tokio::test]
    async fn device_notification_preserves_raw_verification_uri_complete() {
        let raw = "https://www.kimi.com/code/../code?user_code=ABCD-1234";
        assert_ne!(url::Url::parse(raw).expect("URL").to_string(), raw);
        let authorization = start_device_authorization(
            fetch(move |_| {
                Ok(response(
                    200,
                    format!(
                        r#"{{"user_code":"ABCD-1234","device_code":"device-code-123","verification_uri":"https://www.kimi.com/code","verification_uri_complete":"{raw}","interval":5,"expires_in":600}}"#
                    ),
                ))
            }),
            DEFAULT_OAUTH_HOST,
            AbortController::new().signal(),
        )
        .await
        .expect("device authorization");
        assert_eq!(authorization.verification_uri_complete, raw);
    }

    /// Ports pi `test/kimi-coding-oauth.test.ts:121` and `:143`.
    #[tokio::test(start_paused = true)]
    async fn maps_expired_and_denied_device_errors() {
        for (oauth_error, expected) in [("expired_token", "expired"), ("access_denied", "denied")] {
            let replies = Arc::new(Mutex::new(VecDeque::from([
                device(),
                response(400, format!(r#"{{"error":"{oauth_error}"}}"#)),
            ])));
            let fetch = {
                let replies = replies.clone();
                fetch(move |_| Ok(replies.lock().expect("replies").pop_front().expect("reply")))
            };
            let login = (kimi_coding_oauth_with(fetch, Some(DEFAULT_OAUTH_HOST.to_owned())).login)(
                crate::auth::helpers::normalize_interaction(Arc::new(Interaction::default())),
            );
            let task = tokio::spawn(login);
            tokio::time::advance(Duration::from_secs(5)).await;
            assert!(
                task.await
                    .expect("task")
                    .expect_err("failure")
                    .message
                    .contains(expected)
            );
        }
    }

    /// Ports pi `test/kimi-coding-oauth.test.ts:194` and `:231`.
    #[tokio::test(start_paused = true)]
    async fn refresh_retries_429_but_not_invalid_grant_and_to_auth_is_bearer() {
        let replies = Arc::new(Mutex::new(VecDeque::from([
            response(429, r#"{"error":"temporarily_unavailable"}"#),
            response(
                200,
                r#"{"access_token":"a","refresh_token":"r","expires_in":60}"#,
            ),
            response(400, r#"{"error":"invalid_grant"}"#),
        ])));
        let fetch = {
            let replies = replies.clone();
            fetch(move |_| Ok(replies.lock().expect("replies").pop_front().expect("reply")))
        };
        let oauth = kimi_coding_oauth_with(fetch, Some(DEFAULT_OAUTH_HOST.to_owned()));
        let credential = OAuthCredential {
            kind: OAuthCredentialType::OAuth,
            refresh: "old".to_owned(),
            access: "old".to_owned(),
            expires: 0.0,
            extra: Map::new(),
        };
        let refresh = (oauth.refresh)(credential.clone(), AbortController::new().signal());
        let task = tokio::spawn(refresh);
        tokio::time::advance(Duration::from_secs(1)).await;
        let refreshed = task.await.expect("task").expect("refreshed");
        assert_eq!(refreshed.access, "a");
        assert_eq!(
            (oauth.to_auth)(refreshed.clone())
                .await
                .expect("auth")
                .headers
                .expect("headers")["Authorization"]
                .as_deref(),
            Some("Bearer a")
        );
        assert!(
            (oauth.refresh)(credential, AbortController::new().signal())
                .await
                .expect_err("invalid grant")
                .message
                .contains("unauthorized")
        );
    }

    /// Ports pi `test/kimi-coding-oauth.test.ts:165`.
    #[test]
    fn oauth_host_override_prefers_kimi_code_and_trims_trailing_slashes() {
        assert_eq!(
            get_oauth_host_from(|name| match name {
                "KIMI_CODE_OAUTH_HOST" => Some("https://code.example///".to_owned()),
                "KIMI_OAUTH_HOST" => Some("https://legacy.example".to_owned()),
                _ => None,
            }),
            "https://code.example"
        );
        assert_eq!(
            get_oauth_host_from(
                |name| (name == "KIMI_OAUTH_HOST").then(|| "https://legacy.example/".to_owned())
            ),
            "https://legacy.example"
        );
        assert_eq!(
            get_oauth_host_from(|name| match name {
                "KIMI_CODE_OAUTH_HOST" => Some(String::new()),
                "KIMI_OAUTH_HOST" => Some("https://legacy.example/".to_owned()),
                _ => None,
            }),
            "https://legacy.example"
        );
    }
}
