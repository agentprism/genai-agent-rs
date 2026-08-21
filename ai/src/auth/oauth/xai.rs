use super::device_code::{
    OAuthDeviceCodePollOptions, OAuthDeviceCodePollResult, poll_oauth_device_code_flow,
};
use super::{OAuthHttpError, form, now_millis, request, send_http};
use crate::auth::types::{
    AuthError, AuthEvent, AuthFuture, ModelAuth, OAuthAuth, OAuthCredential, OAuthCredentialType,
    ProviderAuthInteraction,
};
use crate::types::{FetchFunction, default_fetch};
use serde_json::{Map, Value};
use std::sync::Arc;

const XAI_CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const XAI_SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";
const XAI_DEVICE_CODE_URL: &str = "https://auth.x.ai/oauth2/device/code";
const XAI_TOKEN_URL: &str = "https://auth.x.ai/oauth2/token";
const REFRESH_SKEW_MS: f64 = 5.0 * 60.0 * 1_000.0;
const DEFAULT_TOKEN_LIFETIME_SECONDS: f64 = 3_600.0;

#[derive(Clone)]
struct XaiEndpoints {
    device_code_url: String,
    token_url: String,
}

struct OAuthHttpResponse {
    ok: bool,
    status: u16,
    body: Map<String, Value>,
}

struct XaiDeviceCode {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: Option<String>,
    interval_seconds: Option<f64>,
    expires_in_seconds: f64,
}

fn required_string(body: &Map<String, Value>, field: &str) -> Result<String, AuthError> {
    body.get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| AuthError::new(format!("Invalid xAI OAuth response field: {field}")))
}

fn positive_number(body: &Map<String, Value>, field: &str) -> Result<f64, AuthError> {
    body.get(field)
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite() && *value > 0.0)
        .ok_or_else(|| AuthError::new(format!("Invalid xAI OAuth response field: {field}")))
}

fn validate_verification_uri(raw: &str) -> Result<String, AuthError> {
    let url = url::Url::parse(raw)
        .map_err(|_| AuthError::new("Untrusted verification URI in xAI OAuth response"))?;
    if url.scheme() != "https" {
        return Err(AuthError::new(
            "Untrusted verification URI in xAI OAuth response",
        ));
    }
    Ok(url.to_string())
}

async fn post_form(
    fetch: Arc<dyn FetchFunction>,
    url: String,
    fields: &[(&str, &str)],
    signal: Arc<dyn crate::types::AbortSignal>,
) -> Result<OAuthHttpResponse, AuthError> {
    let response = send_http(
        fetch,
        request(
            "POST",
            url,
            [
                ("Accept", "application/json"),
                ("Content-Type", "application/x-www-form-urlencoded"),
            ],
            form(fields),
            signal.clone(),
        ),
        None,
    )
    .await
    .map_err(|error| match error {
        OAuthHttpError::Aborted => AuthError::new("Login cancelled"),
        other => AuthError::new(other.to_string()),
    })?;
    let body = serde_json::from_str::<Value>(&response.body)
        .map(|value| value.as_object().cloned().unwrap_or_default())
        .map_err(|_| {
            if signal.is_aborted() {
                AuthError::new("Login cancelled")
            } else {
                AuthError::new(format!(
                    "xAI OAuth returned invalid JSON (HTTP {})",
                    response.status
                ))
            }
        })?;
    Ok(OAuthHttpResponse {
        ok: response.ok(),
        status: response.status,
        body,
    })
}

fn request_failure(action: &str, response: &OAuthHttpResponse) -> AuthError {
    let error = response.body.get("error").and_then(Value::as_str);
    let description = response
        .body
        .get("error_description")
        .and_then(Value::as_str);
    let detail = [error, description]
        .into_iter()
        .flatten()
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>()
        .join(": ");
    AuthError::new(format!(
        "xAI OAuth {action} failed (HTTP {}){}",
        response.status,
        if detail.is_empty() {
            String::new()
        } else {
            format!(": {detail}")
        }
    ))
}

fn parse_device_code(body: &Map<String, Value>) -> Result<XaiDeviceCode, AuthError> {
    let interval_seconds = body
        .get("interval")
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite() && *value > 0.0);
    let verification_uri_complete = body
        .get("verification_uri_complete")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(validate_verification_uri)
        .transpose()?;
    Ok(XaiDeviceCode {
        device_code: required_string(body, "device_code")?,
        user_code: required_string(body, "user_code")?,
        verification_uri: validate_verification_uri(&required_string(body, "verification_uri")?)?,
        verification_uri_complete,
        interval_seconds,
        expires_in_seconds: positive_number(body, "expires_in")?,
    })
}

fn credentials_from_token_response(
    body: &Map<String, Value>,
    previous_refresh_token: Option<&str>,
) -> Result<OAuthCredential, AuthError> {
    let access = required_string(body, "access_token")?;
    let refresh = match (body.get("refresh_token"), previous_refresh_token) {
        (None, Some(previous)) if !previous.is_empty() => previous.to_owned(),
        _ => required_string(body, "refresh_token")?,
    };
    let expires_in_seconds = if body.contains_key("expires_in") {
        positive_number(body, "expires_in")?
    } else {
        DEFAULT_TOKEN_LIFETIME_SECONDS
    };
    Ok(OAuthCredential {
        kind: OAuthCredentialType::OAuth,
        access,
        refresh,
        expires: now_millis() + expires_in_seconds * 1_000.0 - REFRESH_SKEW_MS,
        extra: Map::new(),
    })
}

async fn request_device_code(
    fetch: Arc<dyn FetchFunction>,
    endpoints: XaiEndpoints,
    signal: Arc<dyn crate::types::AbortSignal>,
) -> Result<XaiDeviceCode, AuthError> {
    let response = post_form(
        fetch,
        endpoints.device_code_url,
        &[
            ("client_id", XAI_CLIENT_ID),
            ("scope", XAI_SCOPE),
            ("referrer", "pi"),
        ],
        signal,
    )
    .await?;
    if !response.ok {
        return Err(request_failure("device authorization", &response));
    }
    parse_device_code(&response.body)
}

async fn poll_for_tokens(
    fetch: Arc<dyn FetchFunction>,
    endpoints: XaiEndpoints,
    device: XaiDeviceCode,
    signal: Arc<dyn crate::types::AbortSignal>,
) -> Result<OAuthCredential, AuthError> {
    let poll_signal = signal.clone();
    poll_oauth_device_code_flow(
        OAuthDeviceCodePollOptions {
            interval_seconds: device.interval_seconds,
            expires_in_seconds: Some(device.expires_in_seconds),
            wait_before_first_poll: true,
            signal: signal.clone(),
        },
        move || {
            let fetch = fetch.clone();
            let token_url = endpoints.token_url.clone();
            let device_code = device.device_code.clone();
            let signal = poll_signal.clone();
            async move {
                let response = post_form(
                    fetch,
                    token_url,
                    &[
                        ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                        ("client_id", XAI_CLIENT_ID),
                        ("device_code", &device_code),
                    ],
                    signal,
                )
                .await?;
                if response.ok {
                    return credentials_from_token_response(&response.body, None)
                        .map(OAuthDeviceCodePollResult::Complete);
                }
                Ok(match response.body.get("error").and_then(Value::as_str) {
                    Some("authorization_pending") => OAuthDeviceCodePollResult::Pending,
                    Some("slow_down") => OAuthDeviceCodePollResult::SlowDown {
                        interval_seconds: response.body.get("interval").and_then(Value::as_f64),
                    },
                    Some("access_denied" | "authorization_denied") => {
                        OAuthDeviceCodePollResult::Failed {
                            message: "xAI device authorization was denied".to_owned(),
                        }
                    }
                    Some("expired_token") => OAuthDeviceCodePollResult::Failed {
                        message: "xAI device code expired".to_owned(),
                    },
                    _ => OAuthDeviceCodePollResult::Failed {
                        message: request_failure("device token polling", &response).message,
                    },
                })
            }
        },
    )
    .await
}

async fn login_xai(
    fetch: Arc<dyn FetchFunction>,
    endpoints: XaiEndpoints,
    interaction: ProviderAuthInteraction,
) -> Result<OAuthCredential, AuthError> {
    let device =
        request_device_code(fetch.clone(), endpoints.clone(), interaction.signal.clone()).await?;
    interaction.interaction.notify(AuthEvent::DeviceCode {
        user_code: device.user_code.clone(),
        verification_uri: device
            .verification_uri_complete
            .clone()
            .unwrap_or_else(|| device.verification_uri.clone()),
        interval_seconds: device.interval_seconds,
        expires_in_seconds: Some(device.expires_in_seconds),
    });
    poll_for_tokens(fetch, endpoints, device, interaction.signal).await
}

async fn refresh_xai_token(
    fetch: Arc<dyn FetchFunction>,
    token_url: String,
    refresh_token: String,
    signal: Arc<dyn crate::types::AbortSignal>,
) -> Result<OAuthCredential, AuthError> {
    let response = post_form(
        fetch,
        token_url,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", XAI_CLIENT_ID),
            ("refresh_token", &refresh_token),
        ],
        signal,
    )
    .await?;
    if !response.ok {
        return Err(request_failure("token refresh", &response));
    }
    credentials_from_token_response(&response.body, Some(&refresh_token))
}

fn xai_oauth_with(fetch: Arc<dyn FetchFunction>, endpoints: XaiEndpoints) -> OAuthAuth {
    let login_fetch = fetch.clone();
    let login_endpoints = endpoints.clone();
    let refresh_fetch = fetch;
    let refresh_url = endpoints.token_url;
    OAuthAuth {
        name: "xAI (Grok/X subscription)".to_owned(),
        is_subscription: Some(true),
        login_label: Some("Sign in with SuperGrok or X Premium".to_owned()),
        login: Arc::new(move |interaction| {
            Box::pin(login_xai(
                login_fetch.clone(),
                login_endpoints.clone(),
                interaction,
            )) as AuthFuture<OAuthCredential>
        }),
        refresh: Arc::new(move |credential, signal| {
            Box::pin(refresh_xai_token(
                refresh_fetch.clone(),
                refresh_url.clone(),
                credential.refresh,
                signal,
            )) as AuthFuture<OAuthCredential>
        }),
        to_auth: Arc::new(|credential| {
            Box::pin(async move {
                Ok(ModelAuth {
                    api_key: Some(credential.access),
                    ..ModelAuth::default()
                })
            })
        }),
    }
}

pub fn xai_oauth() -> OAuthAuth {
    xai_oauth_with(
        default_fetch(),
        XaiEndpoints {
            device_code_url: XAI_DEVICE_CODE_URL.to_owned(),
            token_url: XAI_TOKEN_URL.to_owned(),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::oauth::test_support::{fetch, response};
    use crate::auth::types::{AuthInteraction, AuthPrompt};
    use crate::types::ProviderHttpRequest;
    use crate::utils::abort::{AbortController, AbortReason};
    use std::collections::VecDeque;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Interaction {
        events: Mutex<Vec<AuthEvent>>,
        signal: Option<Arc<dyn crate::types::AbortSignal>>,
    }

    impl AuthInteraction for Interaction {
        fn signal(&self) -> Option<Arc<dyn crate::types::AbortSignal>> {
            self.signal.clone()
        }

        fn prompt(&self, _prompt: AuthPrompt) -> AuthFuture<String> {
            Box::pin(async { Err(AuthError::new("Unexpected prompt")) })
        }

        fn notify(&self, event: AuthEvent) {
            self.events.lock().expect("events").push(event);
        }
    }

    fn endpoints() -> XaiEndpoints {
        XaiEndpoints {
            device_code_url: XAI_DEVICE_CODE_URL.to_owned(),
            token_url: XAI_TOKEN_URL.to_owned(),
        }
    }

    fn body(request: &ProviderHttpRequest) -> String {
        String::from_utf8(request.body.clone().expect("request body")).expect("form")
    }

    /// Ports pi `test/xai-oauth.test.ts:85`.
    #[tokio::test(start_paused = true)]
    async fn device_flow_delays_polling_and_handles_pending_and_slow_down() {
        let token_replies = Arc::new(Mutex::new(VecDeque::from([
            response(400, r#"{"error":"authorization_pending"}"#),
            response(400, r#"{"error":"slow_down","interval":10}"#),
            response(
                200,
                r#"{"access_token":"access-token","refresh_token":"refresh-token","expires_in":21600}"#,
            ),
        ])));
        let replies = token_replies.clone();
        let fetch = fetch(move |request| {
            if request.url == XAI_DEVICE_CODE_URL {
                assert!(body(&request).contains("referrer=pi"));
                return Ok(response(
                    200,
                    r#"{"device_code":"device-code","user_code":"ABCD-1234","verification_uri":"https://accounts.x.ai/oauth2/device","expires_in":900,"interval":5}"#,
                ));
            }
            assert!(body(&request).contains("device_code=device-code"));
            Ok(replies.lock().expect("replies").pop_front().expect("reply"))
        });
        let interaction = Arc::new(Interaction::default());
        let login = (xai_oauth_with(fetch, endpoints()).login)(
            crate::auth::helpers::normalize_interaction(interaction.clone()),
        );
        tokio::pin!(login);
        tokio::time::advance(std::time::Duration::from_secs(20)).await;
        let credential = login.await.expect("credential");
        assert_eq!(credential.access, "access-token");
        assert_eq!(credential.refresh, "refresh-token");
        assert!(matches!(
            interaction.events.lock().expect("events").as_slice(),
            [AuthEvent::DeviceCode { verification_uri, interval_seconds: Some(5.0), .. }]
                if verification_uri == "https://accounts.x.ai/oauth2/device"
        ));
    }

    /// Ports pi `test/xai-oauth.test.ts:181` and `:211`.
    #[tokio::test]
    async fn prefers_complete_uri_and_rejects_non_https_verification_urls() {
        let good = parse_device_code(
            serde_json::from_str::<Value>(r#"{"device_code":"d","user_code":"u","verification_uri":"https://x.ai/device","verification_uri_complete":"https://x.ai/device?user_code=u","expires_in":9}"#)
                .expect("json")
                .as_object()
                .expect("object"),
        )
        .expect("device");
        assert_eq!(
            good.verification_uri_complete.as_deref(),
            Some("https://x.ai/device?user_code=u")
        );
        for uri in ["http://x.ai/device", "file:///etc/passwd", "not a url"] {
            assert_eq!(
                validate_verification_uri(uri)
                    .expect_err("untrusted")
                    .message,
                "Untrusted verification URI in xAI OAuth response"
            );
        }
    }

    /// Ports pi `test/xai-oauth.test.ts:275` and `:325`.
    #[tokio::test]
    async fn refresh_rotates_or_preserves_refresh_token_and_surfaces_errors() {
        let replies = Arc::new(Mutex::new(VecDeque::from([
            response(
                200,
                r#"{"access_token":"new-access","refresh_token":"new-refresh","expires_in":3600}"#,
            ),
            response(200, r#"{"access_token":"newer-access"}"#),
            response(
                400,
                r#"{"error":"invalid_grant","error_description":"refresh token revoked"}"#,
            ),
        ])));
        let fetch = {
            let replies = replies.clone();
            fetch(move |_| Ok(replies.lock().expect("replies").pop_front().expect("reply")))
        };
        let oauth = xai_oauth_with(fetch, endpoints());
        let signal = AbortController::new().signal();
        let credential = OAuthCredential {
            kind: OAuthCredentialType::OAuth,
            refresh: "old-refresh".to_owned(),
            access: "old".to_owned(),
            expires: 0.0,
            extra: Map::new(),
        };
        let rotated = (oauth.refresh)(credential.clone(), signal.clone())
            .await
            .expect("rotated");
        assert_eq!(rotated.refresh, "new-refresh");
        let mut preserved_input = credential.clone();
        preserved_input.refresh = "keep-refresh".to_owned();
        let preserved = (oauth.refresh)(preserved_input, signal.clone())
            .await
            .expect("preserved");
        assert_eq!(preserved.refresh, "keep-refresh");
        let error = (oauth.refresh)(credential, signal)
            .await
            .expect_err("failure");
        assert_eq!(
            error.message,
            "xAI OAuth token refresh failed (HTTP 400): invalid_grant: refresh token revoked"
        );
    }

    /// Ports pi `test/xai-oauth.test.ts:260`.
    #[tokio::test]
    async fn cancellation_while_waiting_for_first_poll_uses_login_cancelled() {
        let controller = AbortController::new();
        let signal = controller.signal();
        controller.abort(AbortReason::default_abort());
        let error = poll_oauth_device_code_flow::<(), _, _>(
            OAuthDeviceCodePollOptions {
                interval_seconds: Some(5.0),
                expires_in_seconds: Some(900.0),
                wait_before_first_poll: true,
                signal,
            },
            || async { Ok(OAuthDeviceCodePollResult::Complete(())) },
        )
        .await
        .expect_err("cancelled");
        assert_eq!(error.message, "Login cancelled");
    }

    /// Ports pi `test/xai-oauth.test.ts:158` and `:303`.
    #[test]
    fn zero_interval_uses_default_polling_and_missing_expiry_uses_one_hour() {
        let device = parse_device_code(
            serde_json::from_str::<Value>(r#"{"device_code":"d","user_code":"u","verification_uri":"https://x.ai/device","expires_in":900,"interval":0}"#)
                .expect("json")
                .as_object()
                .expect("object"),
        )
        .expect("device");
        assert_eq!(device.interval_seconds, None);
        let body =
            serde_json::from_str::<Value>(r#"{"access_token":"access","refresh_token":"refresh"}"#)
                .expect("json");
        let before = now_millis();
        let credential = credentials_from_token_response(body.as_object().expect("object"), None)
            .expect("credential");
        assert!(credential.expires >= before + 3_600_000.0 - REFRESH_SKEW_MS);
    }

    /// Ports pi `test/xai-oauth.test.ts:316`.
    #[test]
    fn missing_access_token_is_rejected_by_field_name() {
        let body =
            serde_json::from_str::<Value>(r#"{"refresh_token":"refresh","expires_in":3600}"#)
                .expect("json");
        assert_eq!(
            credentials_from_token_response(body.as_object().expect("object"), None)
                .expect_err("missing access")
                .message,
            "Invalid xAI OAuth response field: access_token"
        );
    }

    /// Pins pi `src/auth/oauth/xai.ts:100-105` and `:128-136`.
    #[test]
    fn empty_error_details_and_previous_refresh_tokens_are_falsy() {
        let response = OAuthHttpResponse {
            ok: false,
            status: 400,
            body: Map::from_iter([
                ("error".to_owned(), Value::String(String::new())),
                (
                    "error_description".to_owned(),
                    Value::String("description".to_owned()),
                ),
            ]),
        };
        assert_eq!(
            request_failure("token refresh", &response).message,
            "xAI OAuth token refresh failed (HTTP 400): description"
        );
        let empty = OAuthHttpResponse {
            ok: false,
            status: 400,
            body: Map::from_iter([
                ("error".to_owned(), Value::String(String::new())),
                ("error_description".to_owned(), Value::String(String::new())),
            ]),
        };
        assert_eq!(
            request_failure("token refresh", &empty).message,
            "xAI OAuth token refresh failed (HTTP 400)"
        );

        let body = serde_json::from_str::<Value>(r#"{"access_token":"access","expires_in":3600}"#)
            .expect("json");
        assert_eq!(
            credentials_from_token_response(body.as_object().expect("object"), Some(""))
                .expect_err("empty stored refresh token")
                .message,
            "Invalid xAI OAuth response field: refresh_token"
        );
    }
}
