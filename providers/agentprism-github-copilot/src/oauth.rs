//! Pinned GitHub Copilot device OAuth and entitlement discovery.

use agentprism_ai::{
    AuthAnswer, AuthChallengeId, AuthError, AuthEvent, AuthInteraction, AuthPrompt, AuthSource,
    CancellationToken, LocalAuthInteraction, LocalBoxFuture, LocalHttpTransport, LocalOAuthAuth,
    LocalOAuthDeviceCodePoll, LocalOAuthDeviceCodePollOptions, ModelId, OAuthAuth, OAuthCredential,
    OAuthDeviceCodePoll, OAuthDeviceCodePollOptions, OAuthDeviceCodePollResult,
    OAuthDeviceCodeRuntime, ProviderOAuthExtra, ResolvedAuth, SecretString, SendBoxFuture,
    SystemOAuthDeviceCodeRuntime, Timestamp, poll_local_oauth_device_code_flow,
    poll_oauth_device_code_flow,
};
use futures_util::future::{Either, select};
use http::{HeaderMap, HeaderValue, Method, header};
use serde_json::Value;
use std::collections::BTreeSet;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;
use std::time::{Instant, SystemTime};
use url::Url;

const CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";
const USER_AGENT: &str = "GitHubCopilotChat/0.35.0";
const API_VERSION: &str = "2026-06-01";
const COPILOT_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const COPILOT_RETRY_BUDGET: Duration = Duration::from_secs(5);

/// Parsed account model availability returned by Copilot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CopilotEntitlements {
    /// Models already visible to the picker.
    pub available_model_ids: Vec<ModelId>,
    /// Known unconfigured models whose policy should be enabled at login.
    pub policy_model_ids: Vec<ModelId>,
}

/// Parses pinned Copilot picker/policy rules from `/models`.
pub fn parse_entitlements(
    raw: &Value,
    allow_policy_fallback: bool,
    known_models: &BTreeSet<ModelId>,
) -> Result<CopilotEntitlements, AuthError> {
    let data = raw
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| AuthError::new("github_copilot_oauth", "Invalid Copilot models response"))?;
    let mut account = Vec::new();
    for item in data {
        let Some(id) = item.get("id").and_then(Value::as_str) else {
            continue;
        };
        if item
            .pointer("/capabilities/supports/tool_calls")
            .and_then(Value::as_bool)
            == Some(false)
        {
            continue;
        }
        account.push((
            ModelId::new(id),
            item.get("model_picker_enabled").and_then(Value::as_bool) == Some(true),
            item.pointer("/policy/state").and_then(Value::as_str),
        ));
    }
    let mut available_model_ids = account
        .iter()
        .filter(|(_, picker, policy)| *picker && *policy != Some("disabled"))
        .map(|(id, _, _)| id.clone())
        .collect::<Vec<_>>();
    let use_policy_fallback = allow_policy_fallback && available_model_ids.is_empty();
    if use_policy_fallback {
        available_model_ids = account
            .iter()
            .filter(|(_, _, policy)| *policy == Some("enabled"))
            .map(|(id, _, _)| id.clone())
            .collect();
    }
    let policy_model_ids = account
        .iter()
        .filter(|(id, picker, policy)| {
            *policy == Some("unconfigured")
                && known_models.contains(id)
                && (*picker || use_policy_fallback)
        })
        .map(|(id, _, _)| id.clone())
        .collect();
    Ok(CopilotEntitlements {
        available_model_ids,
        policy_model_ids,
    })
}

/// Send-capable GitHub Copilot OAuth.
pub struct GitHubCopilotOAuth {
    transport: Arc<dyn agentprism_ai::HttpTransport>,
}

impl GitHubCopilotOAuth {
    /// Creates the concrete flow over the supplied transport.
    pub fn new(transport: Arc<dyn agentprism_ai::HttpTransport>) -> Self {
        Self { transport }
    }
}

impl OAuthAuth for GitHubCopilotOAuth {
    fn name(&self) -> &str {
        "GitHub Copilot"
    }

    fn login(
        &self,
        interaction: Arc<dyn AuthInteraction>,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<OAuthCredential, AuthError>> {
        Box::pin(async move {
            let enterprise = prompt_domain_send(interaction.as_ref(), cancellation.clone()).await?;
            let domain = enterprise.as_deref().unwrap_or("github.com");
            let device =
                start_device_send(self.transport.as_ref(), domain, cancellation.clone()).await?;
            interaction.notify(device.event())?;
            let poll = CopilotPoll {
                transport: Arc::clone(&self.transport),
                url: access_token_url(domain)?,
                device_code: device.device_code,
            };
            let mut options = OAuthDeviceCodePollOptions::new(Box::new(poll), cancellation.clone());
            options.interval = device.interval;
            options.expires_in = Some(device.expires_in);
            options.wait_before_first_poll = true;
            let github_token = poll_oauth_device_code_flow(options).await?;
            login_finish_send(
                self.transport.as_ref(),
                github_token,
                enterprise,
                Some(interaction.as_ref()),
                cancellation,
            )
            .await
        })
    }

    fn refresh(
        &self,
        credential: OAuthCredential,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<OAuthCredential, AuthError>> {
        Box::pin(async move {
            let enterprise = enterprise_from(&credential);
            let account_id = account_id_from(&credential);
            let mut refreshed = exchange_copilot_send(
                self.transport.as_ref(),
                credential.refresh.clone(),
                enterprise.as_deref(),
                cancellation.clone(),
            )
            .await?;
            set_account_id(&mut refreshed, account_id);
            let entitlements = fetch_models_send(
                self.transport.as_ref(),
                refreshed.access.expose_secret(),
                enterprise.as_deref(),
                cancellation,
                0,
            )
            .await?;
            set_available(&mut refreshed, entitlements.available_model_ids);
            Ok(refreshed)
        })
    }

    fn to_auth(
        &self,
        credential: &OAuthCredential,
    ) -> SendBoxFuture<'_, Result<ResolvedAuth, AuthError>> {
        let result = copilot_auth(credential);
        Box::pin(async move { result })
    }
}

/// Local-executor GitHub Copilot OAuth.
pub struct LocalGitHubCopilotOAuth {
    transport: Rc<dyn LocalHttpTransport>,
}

impl LocalGitHubCopilotOAuth {
    /// Creates the local concrete flow.
    pub fn new(transport: Rc<dyn LocalHttpTransport>) -> Self {
        Self { transport }
    }
}

impl LocalOAuthAuth for LocalGitHubCopilotOAuth {
    fn name(&self) -> &str {
        "GitHub Copilot"
    }

    fn login(
        &self,
        interaction: Rc<dyn LocalAuthInteraction>,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<OAuthCredential, AuthError>> {
        Box::pin(async move {
            let enterprise =
                prompt_domain_local(interaction.as_ref(), cancellation.clone()).await?;
            let domain = enterprise.as_deref().unwrap_or("github.com");
            let device =
                start_device_local(self.transport.as_ref(), domain, cancellation.clone()).await?;
            interaction.notify(device.event())?;
            let poll = LocalCopilotPoll {
                transport: Rc::clone(&self.transport),
                url: access_token_url(domain)?,
                device_code: device.device_code,
            };
            let mut options =
                LocalOAuthDeviceCodePollOptions::new(Box::new(poll), cancellation.clone());
            options.interval = device.interval;
            options.expires_in = Some(device.expires_in);
            options.wait_before_first_poll = true;
            let github_token = poll_local_oauth_device_code_flow(options).await?;
            login_finish_local(
                self.transport.as_ref(),
                github_token,
                enterprise,
                Some(interaction.as_ref()),
                cancellation,
            )
            .await
        })
    }

    fn refresh(
        &self,
        credential: OAuthCredential,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<OAuthCredential, AuthError>> {
        Box::pin(async move {
            let enterprise = enterprise_from(&credential);
            let account_id = account_id_from(&credential);
            let mut refreshed = exchange_copilot_local(
                self.transport.as_ref(),
                credential.refresh.clone(),
                enterprise.as_deref(),
                cancellation.clone(),
            )
            .await?;
            set_account_id(&mut refreshed, account_id);
            let entitlements = fetch_models_local(
                self.transport.as_ref(),
                refreshed.access.expose_secret(),
                enterprise.as_deref(),
                cancellation,
                0,
            )
            .await?;
            set_available(&mut refreshed, entitlements.available_model_ids);
            Ok(refreshed)
        })
    }

    fn to_auth(
        &self,
        credential: &OAuthCredential,
    ) -> LocalBoxFuture<'_, Result<ResolvedAuth, AuthError>> {
        let result = copilot_auth(credential);
        Box::pin(async move { result })
    }
}

#[derive(Clone)]
struct Device {
    device_code: String,
    user_code: String,
    verification_uri: Url,
    interval: Option<Duration>,
    expires_in: Duration,
}

impl Device {
    fn event(&self) -> AuthEvent {
        AuthEvent::DeviceCode {
            challenge_id: AuthChallengeId::new(self.device_code.clone()),
            user_code: self.user_code.clone(),
            verification_uri: self.verification_uri.clone(),
            interval: self.interval,
            expires_in: Some(self.expires_in),
        }
    }
}

struct CopilotPoll {
    transport: Arc<dyn agentprism_ai::HttpTransport>,
    url: Url,
    device_code: String,
}

impl OAuthDeviceCodePoll<SecretString> for CopilotPoll {
    fn poll(
        &mut self,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<OAuthDeviceCodePollResult<SecretString>, AuthError>> {
        Box::pin(async move {
            let request = github_form(
                self.url.clone(),
                &[
                    ("client_id", CLIENT_ID),
                    ("device_code", &self.device_code),
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ],
            )?;
            let (status, _, body) = agentprism_provider_common::execute_send(
                self.transport.as_ref(),
                request,
                cancellation,
            )
            .await?;
            parse_device_token(status, &body)
        })
    }
}

struct LocalCopilotPoll {
    transport: Rc<dyn LocalHttpTransport>,
    url: Url,
    device_code: String,
}

impl LocalOAuthDeviceCodePoll<SecretString> for LocalCopilotPoll {
    fn poll(
        &mut self,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<OAuthDeviceCodePollResult<SecretString>, AuthError>> {
        Box::pin(async move {
            let request = github_form(
                self.url.clone(),
                &[
                    ("client_id", CLIENT_ID),
                    ("device_code", &self.device_code),
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ],
            )?;
            let (status, _, body) = agentprism_provider_common::execute_local(
                self.transport.as_ref(),
                request,
                cancellation,
            )
            .await?;
            parse_device_token(status, &body)
        })
    }
}

async fn prompt_domain_send(
    interaction: &dyn AuthInteraction,
    cancellation: CancellationToken,
) -> Result<Option<String>, AuthError> {
    match interaction.prompt(domain_prompt(), cancellation).await? {
        AuthAnswer::Text(value) => normalize_domain(&value),
        _ => Err(AuthError::new(
            "github_copilot_oauth",
            "enterprise-domain prompt returned a non-text answer",
        )),
    }
}

async fn prompt_domain_local(
    interaction: &dyn LocalAuthInteraction,
    cancellation: CancellationToken,
) -> Result<Option<String>, AuthError> {
    match interaction.prompt(domain_prompt(), cancellation).await? {
        AuthAnswer::Text(value) => normalize_domain(&value),
        _ => Err(AuthError::new(
            "github_copilot_oauth",
            "enterprise-domain prompt returned a non-text answer",
        )),
    }
}

fn domain_prompt() -> AuthPrompt {
    AuthPrompt::Text {
        message: "GitHub Enterprise URL/domain (blank for github.com)".into(),
        placeholder: Some("company.ghe.com".into()),
    }
}

fn normalize_domain(input: &str) -> Result<Option<String>, AuthError> {
    let value = input.trim();
    if value.is_empty() {
        return Ok(None);
    }
    let candidate = if value.contains("://") {
        value.to_owned()
    } else {
        format!("https://{value}")
    };
    Url::parse(&candidate)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .map(Some)
        .ok_or_else(|| {
            AuthError::new(
                "github_copilot_oauth",
                "Invalid GitHub Enterprise URL/domain",
            )
        })
}

async fn start_device_send(
    transport: &dyn agentprism_ai::HttpTransport,
    domain: &str,
    cancellation: CancellationToken,
) -> Result<Device, AuthError> {
    let request = github_form(
        device_url(domain)?,
        &[("client_id", CLIENT_ID), ("scope", "read:user")],
    )?;
    let (status, _, body) =
        agentprism_provider_common::execute_send(transport, request, cancellation).await?;
    parse_device(status, &body)
}

async fn start_device_local(
    transport: &dyn LocalHttpTransport,
    domain: &str,
    cancellation: CancellationToken,
) -> Result<Device, AuthError> {
    let request = github_form(
        device_url(domain)?,
        &[("client_id", CLIENT_ID), ("scope", "read:user")],
    )?;
    let (status, _, body) =
        agentprism_provider_common::execute_local(transport, request, cancellation).await?;
    parse_device(status, &body)
}

fn parse_device(status: u16, body: &[u8]) -> Result<Device, AuthError> {
    let value = ok_json(status, body)?;
    let string = |name: &str| {
        value
            .get(name)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| {
                AuthError::new(
                    "github_copilot_oauth",
                    "Invalid device code response fields",
                )
            })
    };
    let verification_uri = Url::parse(&string("verification_uri")?).map_err(|_| {
        AuthError::new(
            "github_copilot_oauth",
            "Untrusted verification_uri in device code response",
        )
    })?;
    if !matches!(verification_uri.scheme(), "http" | "https") {
        return Err(AuthError::new(
            "github_copilot_oauth",
            "Untrusted verification_uri in device code response",
        ));
    }
    let expires = value
        .get("expires_in")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            AuthError::new(
                "github_copilot_oauth",
                "Invalid device code response fields",
            )
        })?;
    Ok(Device {
        device_code: string("device_code")?,
        user_code: string("user_code")?,
        verification_uri,
        interval: value
            .get("interval")
            .and_then(Value::as_u64)
            .map(Duration::from_secs),
        expires_in: Duration::from_secs(expires),
    })
}

fn parse_device_token(
    status: u16,
    body: &[u8],
) -> Result<OAuthDeviceCodePollResult<SecretString>, AuthError> {
    let value = ok_json(status, body)?;
    if let Some(access) = value.get("access_token").and_then(Value::as_str) {
        return Ok(OAuthDeviceCodePollResult::Complete(SecretString::new(
            access,
        )));
    }
    let error = value.get("error").and_then(Value::as_str).unwrap_or("");
    Ok(match error {
        "authorization_pending" => OAuthDeviceCodePollResult::Pending,
        "slow_down" => OAuthDeviceCodePollResult::SlowDown {
            interval: value
                .get("interval")
                .and_then(Value::as_u64)
                .map(Duration::from_secs),
        },
        "" => OAuthDeviceCodePollResult::Failed {
            message: "Invalid device token response".into(),
        },
        other => {
            let suffix = value
                .get("error_description")
                .and_then(Value::as_str)
                .map(|value| format!(": {value}"))
                .unwrap_or_default();
            OAuthDeviceCodePollResult::Failed {
                message: format!("Device flow failed: {other}{suffix}"),
            }
        }
    })
}

async fn login_finish_send(
    transport: &dyn agentprism_ai::HttpTransport,
    github_token: SecretString,
    enterprise: Option<String>,
    interaction: Option<&dyn AuthInteraction>,
    cancellation: CancellationToken,
) -> Result<OAuthCredential, AuthError> {
    let mut credential = exchange_copilot_send(
        transport,
        github_token,
        enterprise.as_deref(),
        cancellation.clone(),
    )
    .await?;
    let entitlements = fetch_models_send(
        transport,
        credential.access.expose_secret(),
        enterprise.as_deref(),
        cancellation.clone(),
        2,
    )
    .await?;
    if !entitlements.policy_model_ids.is_empty()
        && let Some(host) = interaction
    {
        host.notify(AuthEvent::Progress {
            message: "Enabling models...".into(),
        })?;
    }
    let mut available = entitlements.available_model_ids;
    for id in entitlements.policy_model_ids {
        match enable_model_send(
            transport,
            credential.access.expose_secret(),
            enterprise.as_deref(),
            &id,
            cancellation.clone(),
        )
        .await
        {
            Ok(true) => available.push(id),
            Ok(false) => {}
            Err(error) if error.code() == "github_copilot_policy_rate_limited" => break,
            Err(_) if cancellation.is_cancelled() => return Err(AuthError::Cancelled),
            Err(_) => {}
        }
    }
    let mut seen = BTreeSet::new();
    available.retain(|id| seen.insert(id.clone()));
    set_available(&mut credential, available);
    Ok(credential)
}

async fn login_finish_local(
    transport: &dyn LocalHttpTransport,
    github_token: SecretString,
    enterprise: Option<String>,
    interaction: Option<&dyn LocalAuthInteraction>,
    cancellation: CancellationToken,
) -> Result<OAuthCredential, AuthError> {
    let mut credential = exchange_copilot_local(
        transport,
        github_token,
        enterprise.as_deref(),
        cancellation.clone(),
    )
    .await?;
    let entitlements = fetch_models_local(
        transport,
        credential.access.expose_secret(),
        enterprise.as_deref(),
        cancellation.clone(),
        2,
    )
    .await?;
    if !entitlements.policy_model_ids.is_empty()
        && let Some(host) = interaction
    {
        host.notify(AuthEvent::Progress {
            message: "Enabling models...".into(),
        })?;
    }
    let mut available = entitlements.available_model_ids;
    for id in entitlements.policy_model_ids {
        match enable_model_local(
            transport,
            credential.access.expose_secret(),
            enterprise.as_deref(),
            &id,
            cancellation.clone(),
        )
        .await
        {
            Ok(true) => available.push(id),
            Ok(false) => {}
            Err(error) if error.code() == "github_copilot_policy_rate_limited" => break,
            Err(_) if cancellation.is_cancelled() => return Err(AuthError::Cancelled),
            Err(_) => {}
        }
    }
    let mut seen = BTreeSet::new();
    available.retain(|id| seen.insert(id.clone()));
    set_available(&mut credential, available);
    Ok(credential)
}

async fn exchange_copilot_send(
    transport: &dyn agentprism_ai::HttpTransport,
    github_token: SecretString,
    enterprise: Option<&str>,
    cancellation: CancellationToken,
) -> Result<OAuthCredential, AuthError> {
    let request = bearer_get(
        copilot_token_url(enterprise.unwrap_or("github.com"))?,
        github_token.expose_secret(),
        false,
    )?;
    let (status, _, body) =
        agentprism_provider_common::execute_send(transport, request, cancellation).await?;
    parse_copilot_token(status, &body, github_token, enterprise)
}

async fn exchange_copilot_local(
    transport: &dyn LocalHttpTransport,
    github_token: SecretString,
    enterprise: Option<&str>,
    cancellation: CancellationToken,
) -> Result<OAuthCredential, AuthError> {
    let request = bearer_get(
        copilot_token_url(enterprise.unwrap_or("github.com"))?,
        github_token.expose_secret(),
        false,
    )?;
    let (status, _, body) =
        agentprism_provider_common::execute_local(transport, request, cancellation).await?;
    parse_copilot_token(status, &body, github_token, enterprise)
}

fn parse_copilot_token(
    status: u16,
    body: &[u8],
    refresh: SecretString,
    enterprise: Option<&str>,
) -> Result<OAuthCredential, AuthError> {
    let value = ok_json(status, body)?;
    let token = value.get("token").and_then(Value::as_str).ok_or_else(|| {
        AuthError::new(
            "github_copilot_oauth",
            "Invalid Copilot token response fields",
        )
    })?;
    let expiry = value
        .get("expires_at")
        .and_then(Value::as_i64)
        .ok_or_else(|| {
            AuthError::new(
                "github_copilot_oauth",
                "Invalid Copilot token response fields",
            )
        })?;
    let api_endpoint = base_url_from_token(token)
        .or_else(|| {
            enterprise.and_then(|domain| Url::parse(&format!("https://copilot-api.{domain}")).ok())
        })
        .unwrap_or_else(|| {
            Url::parse("https://api.individual.githubcopilot.com").expect("pinned URL")
        });
    Ok(OAuthCredential {
        access: SecretString::new(token),
        refresh,
        expires_at: Timestamp::from_unix_millis(
            expiry.saturating_mul(1000).saturating_sub(300_000),
        ),
        extra: ProviderOAuthExtra::GitHubCopilot {
            api_endpoint,
            account_id: None,
            enterprise_url: enterprise.map(str::to_owned),
            available_model_ids: None,
        },
    })
}

async fn fetch_models_send(
    transport: &dyn agentprism_ai::HttpTransport,
    token: &str,
    enterprise: Option<&str>,
    cancellation: CancellationToken,
    max_retries: u32,
) -> Result<CopilotEntitlements, AuthError> {
    let base = effective_base(token, enterprise)?;
    let url = base.join("models").map_err(url_error)?;
    let started = Instant::now();
    for retry in 0..=max_retries {
        let (status, headers, body) = execute_copilot_send(
            transport,
            bearer_get(url.clone(), token, true)?,
            cancellation.clone(),
            retry_budget_remaining(started, max_retries)?,
        )
        .await?;
        if status != 429 || retry == max_retries {
            return parse_models_response(
                status,
                &body,
                base.as_str() == "https://api.individual.githubcopilot.com/",
            );
        }
        let remaining = COPILOT_RETRY_BUDGET.saturating_sub(started.elapsed());
        let Some(delay) = retry_delay(&headers, retry) else {
            return parse_models_response(
                status,
                &body,
                base.as_str() == "https://api.individual.githubcopilot.com/",
            );
        };
        if delay >= remaining {
            return parse_models_response(
                status,
                &body,
                base.as_str() == "https://api.individual.githubcopilot.com/",
            );
        }
        OAuthDeviceCodeRuntime::sleep(&SystemOAuthDeviceCodeRuntime, delay, cancellation.clone())
            .await?;
    }
    unreachable!("bounded Copilot model-catalog retry loop")
}

async fn fetch_models_local(
    transport: &dyn LocalHttpTransport,
    token: &str,
    enterprise: Option<&str>,
    cancellation: CancellationToken,
    max_retries: u32,
) -> Result<CopilotEntitlements, AuthError> {
    let base = effective_base(token, enterprise)?;
    let url = base.join("models").map_err(url_error)?;
    let started = Instant::now();
    for retry in 0..=max_retries {
        let (status, headers, body) = execute_copilot_local(
            transport,
            bearer_get(url.clone(), token, true)?,
            cancellation.clone(),
            retry_budget_remaining(started, max_retries)?,
        )
        .await?;
        if status != 429 || retry == max_retries {
            return parse_models_response(
                status,
                &body,
                base.as_str() == "https://api.individual.githubcopilot.com/",
            );
        }
        let remaining = COPILOT_RETRY_BUDGET.saturating_sub(started.elapsed());
        let Some(delay) = retry_delay(&headers, retry) else {
            return parse_models_response(
                status,
                &body,
                base.as_str() == "https://api.individual.githubcopilot.com/",
            );
        };
        if delay >= remaining {
            return parse_models_response(
                status,
                &body,
                base.as_str() == "https://api.individual.githubcopilot.com/",
            );
        }
        agentprism_ai::LocalOAuthDeviceCodeRuntime::sleep(
            &SystemOAuthDeviceCodeRuntime,
            delay,
            cancellation.clone(),
        )
        .await?;
    }
    unreachable!("bounded Copilot model-catalog retry loop")
}

fn parse_models_response(
    status: u16,
    body: &[u8],
    fallback: bool,
) -> Result<CopilotEntitlements, AuthError> {
    let value = ok_json(status, body)?;
    let known = super::models()
        .map_err(|error| AuthError::new("github_copilot_oauth", error.to_string()))?
        .into_iter()
        .map(|model| model.common.model_ref.model)
        .collect();
    parse_entitlements(&value, fallback, &known)
}

async fn enable_model_send(
    transport: &dyn agentprism_ai::HttpTransport,
    token: &str,
    enterprise: Option<&str>,
    id: &ModelId,
    cancellation: CancellationToken,
) -> Result<bool, AuthError> {
    let url = effective_base(token, enterprise)?
        .join(&format!("models/{}/policy", id.as_str()))
        .map_err(url_error)?;
    let started = Instant::now();
    for retry in 0..=2 {
        let (status, headers, body) = execute_copilot_send(
            transport,
            policy_request(url.clone(), token)?,
            cancellation.clone(),
            retry_budget_remaining(started, 2)?,
        )
        .await?;
        if status != 429 {
            return Ok((200..300).contains(&status));
        }
        let remaining = COPILOT_RETRY_BUDGET.saturating_sub(started.elapsed());
        if retry == 2 {
            return Err(policy_rate_limit_error(status, &body));
        }
        let Some(delay) = retry_delay(&headers, retry) else {
            return Err(policy_rate_limit_error(status, &body));
        };
        if delay >= remaining {
            return Err(policy_rate_limit_error(status, &body));
        }
        OAuthDeviceCodeRuntime::sleep(&SystemOAuthDeviceCodeRuntime, delay, cancellation.clone())
            .await?;
    }
    unreachable!("bounded Copilot policy retry loop")
}

async fn enable_model_local(
    transport: &dyn LocalHttpTransport,
    token: &str,
    enterprise: Option<&str>,
    id: &ModelId,
    cancellation: CancellationToken,
) -> Result<bool, AuthError> {
    let url = effective_base(token, enterprise)?
        .join(&format!("models/{}/policy", id.as_str()))
        .map_err(url_error)?;
    let started = Instant::now();
    for retry in 0..=2 {
        let (status, headers, body) = execute_copilot_local(
            transport,
            policy_request(url.clone(), token)?,
            cancellation.clone(),
            retry_budget_remaining(started, 2)?,
        )
        .await?;
        if status != 429 {
            return Ok((200..300).contains(&status));
        }
        let remaining = COPILOT_RETRY_BUDGET.saturating_sub(started.elapsed());
        if retry == 2 {
            return Err(policy_rate_limit_error(status, &body));
        }
        let Some(delay) = retry_delay(&headers, retry) else {
            return Err(policy_rate_limit_error(status, &body));
        };
        if delay >= remaining {
            return Err(policy_rate_limit_error(status, &body));
        }
        agentprism_ai::LocalOAuthDeviceCodeRuntime::sleep(
            &SystemOAuthDeviceCodeRuntime,
            delay,
            cancellation.clone(),
        )
        .await?;
    }
    unreachable!("bounded Copilot policy retry loop")
}

fn retry_delay(headers: &HeaderMap, retry: u32) -> Option<Duration> {
    let fallback = Duration::from_millis(500_u64.saturating_mul(1_u64 << retry));
    let Some(value) = headers
        .get(header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
    else {
        return Some(fallback);
    };
    if let Ok(seconds) = value.parse::<f64>()
        && seconds.is_finite()
    {
        return Some(Duration::from_secs_f64(seconds.max(0.0)));
    }
    let date = httpdate::parse_http_date(value).ok()?;
    Some(date.duration_since(SystemTime::now()).unwrap_or_default())
}

fn retry_budget_remaining(
    started: Instant,
    max_retries: u32,
) -> Result<Option<Duration>, AuthError> {
    if max_retries == 0 {
        return Ok(None);
    }
    let remaining = COPILOT_RETRY_BUDGET.saturating_sub(started.elapsed());
    if remaining.is_zero() {
        return Err(copilot_request_timeout());
    }
    Ok(Some(remaining))
}

async fn execute_copilot_send(
    transport: &dyn agentprism_ai::HttpTransport,
    request: agentprism_ai::HttpRequest,
    cancellation: CancellationToken,
    retry_budget_remaining: Option<Duration>,
) -> Result<(u16, HeaderMap, Vec<u8>), AuthError> {
    let timeout = effective_request_timeout(&request, retry_budget_remaining)?;
    let request_cancellation = cancellation.child();
    let execution = Box::pin(agentprism_provider_common::execute_send(
        transport,
        request,
        request_cancellation.clone(),
    ));
    let timer = Box::pin(futures_timer::Delay::new(timeout));
    match select(execution, timer).await {
        Either::Left((result, _)) => result,
        Either::Right(((), _)) => {
            request_cancellation.cancel();
            if cancellation.is_cancelled() {
                Err(AuthError::Cancelled)
            } else {
                Err(copilot_request_timeout())
            }
        }
    }
}

async fn execute_copilot_local(
    transport: &dyn LocalHttpTransport,
    request: agentprism_ai::HttpRequest,
    cancellation: CancellationToken,
    retry_budget_remaining: Option<Duration>,
) -> Result<(u16, HeaderMap, Vec<u8>), AuthError> {
    let timeout = effective_request_timeout(&request, retry_budget_remaining)?;
    let request_cancellation = cancellation.child();
    let execution = Box::pin(agentprism_provider_common::execute_local(
        transport,
        request,
        request_cancellation.clone(),
    ));
    let timer = Box::pin(futures_timer::Delay::new(timeout));
    match select(execution, timer).await {
        Either::Left((result, _)) => result,
        Either::Right(((), _)) => {
            request_cancellation.cancel();
            if cancellation.is_cancelled() {
                Err(AuthError::Cancelled)
            } else {
                Err(copilot_request_timeout())
            }
        }
    }
}

fn effective_request_timeout(
    request: &agentprism_ai::HttpRequest,
    retry_budget_remaining: Option<Duration>,
) -> Result<Duration, AuthError> {
    let timeout = retry_budget_remaining.map_or_else(
        || request.timeout.unwrap_or(COPILOT_REQUEST_TIMEOUT),
        |remaining| {
            request
                .timeout
                .unwrap_or(COPILOT_REQUEST_TIMEOUT)
                .min(remaining)
        },
    );
    if timeout.is_zero() {
        Err(copilot_request_timeout())
    } else {
        Ok(timeout)
    }
}

fn copilot_request_timeout() -> AuthError {
    AuthError::new(
        "github_copilot_request_timeout",
        "GitHub Copilot request timed out after 5000 ms",
    )
}

fn policy_rate_limit_error(status: u16, body: &[u8]) -> AuthError {
    AuthError::new(
        "github_copilot_policy_rate_limited",
        format!(
            "GitHub Copilot returned HTTP {status}: {}",
            String::from_utf8_lossy(body)
        ),
    )
}

fn set_available(credential: &mut OAuthCredential, available: Vec<ModelId>) {
    if let ProviderOAuthExtra::GitHubCopilot {
        available_model_ids,
        ..
    } = &mut credential.extra
    {
        *available_model_ids = Some(available);
    }
}

fn set_account_id(credential: &mut OAuthCredential, account_id: Option<String>) {
    if let ProviderOAuthExtra::GitHubCopilot {
        account_id: current,
        ..
    } = &mut credential.extra
    {
        *current = account_id;
    }
}

fn account_id_from(credential: &OAuthCredential) -> Option<String> {
    match &credential.extra {
        ProviderOAuthExtra::GitHubCopilot { account_id, .. } => account_id.clone(),
        _ => None,
    }
}

fn enterprise_from(credential: &OAuthCredential) -> Option<String> {
    match &credential.extra {
        ProviderOAuthExtra::GitHubCopilot { enterprise_url, .. } => enterprise_url
            .as_deref()
            .and_then(|value| normalize_domain(value).ok().flatten()),
        _ => None,
    }
}

fn copilot_auth(credential: &OAuthCredential) -> Result<ResolvedAuth, AuthError> {
    let ProviderOAuthExtra::GitHubCopilot { .. } = &credential.extra else {
        return Err(AuthError::new(
            "github_copilot_oauth",
            "GitHub Copilot credential has invalid provider metadata",
        ));
    };
    let enterprise = enterprise_from(credential);
    Ok(ResolvedAuth {
        api_key: Some(credential.access.clone()),
        headers: HeaderMap::new(),
        transport_headers: HeaderMap::new(),
        base_url: Some(effective_base(
            credential.access.expose_secret(),
            enterprise.as_deref(),
        )?),
        environment: Default::default(),
        source: AuthSource::new("OAuth"),
    })
}

fn effective_base(token: &str, enterprise: Option<&str>) -> Result<Url, AuthError> {
    base_url_from_token(token)
        .or_else(|| {
            enterprise.and_then(|domain| Url::parse(&format!("https://copilot-api.{domain}")).ok())
        })
        .or_else(|| Url::parse("https://api.individual.githubcopilot.com").ok())
        .ok_or_else(|| AuthError::new("github_copilot_oauth", "invalid Copilot API endpoint"))
}

fn base_url_from_token(token: &str) -> Option<Url> {
    let host = token
        .split(';')
        .find_map(|part| part.strip_prefix("proxy-ep="))?;
    Url::parse(&format!(
        "https://{}",
        host.strip_prefix("proxy.")
            .map_or_else(|| host.to_owned(), |tail| format!("api.{tail}"))
    ))
    .ok()
}

fn device_url(domain: &str) -> Result<Url, AuthError> {
    Url::parse(&format!("https://{domain}/login/device/code")).map_err(url_error)
}

fn access_token_url(domain: &str) -> Result<Url, AuthError> {
    Url::parse(&format!("https://{domain}/login/oauth/access_token")).map_err(url_error)
}

fn copilot_token_url(domain: &str) -> Result<Url, AuthError> {
    Url::parse(&format!("https://api.{domain}/copilot_internal/v2/token")).map_err(url_error)
}

fn url_error(error: url::ParseError) -> AuthError {
    AuthError::new(
        "github_copilot_oauth",
        format!("invalid GitHub Copilot URL: {error}"),
    )
}

fn github_form(url: Url, fields: &[(&str, &str)]) -> Result<agentprism_ai::HttpRequest, AuthError> {
    let mut request = agentprism_provider_common::form_post(url.as_str(), fields)?;
    request
        .headers
        .insert(header::USER_AGENT, HeaderValue::from_static(USER_AGENT));
    Ok(request)
}

fn bearer_get(
    url: Url,
    token: &str,
    models: bool,
) -> Result<agentprism_ai::HttpRequest, AuthError> {
    let mut headers = copilot_headers();
    headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| AuthError::new("github_copilot_oauth", "invalid Copilot token"))?,
    );
    if models {
        headers.insert(
            "x-github-api-version",
            HeaderValue::from_static(API_VERSION),
        );
    }
    Ok(agentprism_ai::HttpRequest {
        method: Method::GET,
        url,
        auth_headers: HeaderMap::new(),
        headers,
        session_id: None,
        body: Vec::new(),
        timeout: models.then_some(COPILOT_REQUEST_TIMEOUT),
        transport: None,
        websocket_connect_timeout: None,
        attempt: 0,
    })
}

fn policy_request(url: Url, token: &str) -> Result<agentprism_ai::HttpRequest, AuthError> {
    let mut headers = copilot_headers();
    headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| AuthError::new("github_copilot_oauth", "invalid Copilot token"))?,
    );
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers.insert("openai-intent", HeaderValue::from_static("chat-policy"));
    headers.insert(
        "x-interaction-type",
        HeaderValue::from_static("chat-policy"),
    );
    Ok(agentprism_ai::HttpRequest {
        method: Method::POST,
        url,
        auth_headers: HeaderMap::new(),
        headers,
        session_id: None,
        body: br#"{"state":"enabled"}"#.to_vec(),
        timeout: Some(COPILOT_REQUEST_TIMEOUT),
        transport: None,
        websocket_connect_timeout: None,
        attempt: 0,
    })
}

fn copilot_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(header::ACCEPT, HeaderValue::from_static("application/json"));
    headers.insert(header::USER_AGENT, HeaderValue::from_static(USER_AGENT));
    headers.insert("editor-version", HeaderValue::from_static("vscode/1.107.0"));
    headers.insert(
        "editor-plugin-version",
        HeaderValue::from_static("copilot-chat/0.35.0"),
    );
    headers.insert(
        "copilot-integration-id",
        HeaderValue::from_static("vscode-chat"),
    );
    headers
}

fn ok_json(status: u16, body: &[u8]) -> Result<Value, AuthError> {
    if !(200..300).contains(&status) {
        return Err(AuthError::new(
            "github_copilot_oauth",
            format!(
                "GitHub Copilot returned HTTP {status}: {}",
                String::from_utf8_lossy(body)
            ),
        ));
    }
    serde_json::from_slice(body).map_err(|error| {
        AuthError::new(
            "github_copilot_oauth",
            format!("invalid GitHub Copilot JSON: {error}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentprism_ai::{
        HttpResponse, LocalBoxFuture, LocalHttpResponse, LocalHttpTransport, SendBoxFuture,
        TransportError,
    };
    use futures_util::{future, stream};

    #[derive(Clone, Copy)]
    enum StallAt {
        Establishment,
        Body,
    }

    struct StalledTransport(StallAt);

    impl agentprism_ai::HttpTransport for StalledTransport {
        fn execute(
            &self,
            _request: agentprism_ai::HttpRequest,
            _cancellation: CancellationToken,
        ) -> SendBoxFuture<'_, Result<HttpResponse, TransportError>> {
            let stall = self.0;
            Box::pin(async move {
                match stall {
                    StallAt::Establishment => future::pending().await,
                    StallAt::Body => Ok(HttpResponse {
                        status: 200,
                        headers: HeaderMap::new(),
                        diagnostics: Vec::new(),
                        notify_observers: true,
                        decode_non_success: false,
                        body: Box::pin(stream::pending()),
                    }),
                }
            })
        }
    }

    impl LocalHttpTransport for StalledTransport {
        fn execute(
            &self,
            _request: agentprism_ai::HttpRequest,
            _cancellation: CancellationToken,
        ) -> LocalBoxFuture<'_, Result<LocalHttpResponse, TransportError>> {
            let stall = self.0;
            Box::pin(async move {
                match stall {
                    StallAt::Establishment => future::pending().await,
                    StallAt::Body => Ok(LocalHttpResponse {
                        status: 200,
                        headers: HeaderMap::new(),
                        diagnostics: Vec::new(),
                        notify_observers: true,
                        decode_non_success: false,
                        body: Box::pin(stream::pending()),
                    }),
                }
            })
        }
    }

    fn short_deadline_request() -> agentprism_ai::HttpRequest {
        let mut request = policy_request(
            Url::parse("https://api.individual.githubcopilot.com/models/test/policy").unwrap(),
            "fixture-token",
        )
        .unwrap();
        request.timeout = Some(Duration::from_millis(10));
        request
    }

    #[test]
    fn github_copilot_entitlement_policy_matrix_pi_exact() {
        // Architecture v2 part 2 §5.2, §6.6, and §10.7; Pi basis:
        // packages/ai/test/github-copilot-oauth.test.ts picker fallback,
        // non-Individual exclusion, known-model policy, and tool capability
        // scenarios.
        let known = ["gpt-4.1", "claude-sonnet-4.5", "gpt-5.4"]
            .into_iter()
            .map(ModelId::new)
            .collect::<BTreeSet<_>>();
        let parse = |data: Value, fallback| {
            parse_entitlements(&serde_json::json!({"data": data}), fallback, &known)
                .expect("Copilot entitlement fixture")
        };

        let picker = parse(
            serde_json::json!([
                {"id":"gpt-4.1","model_picker_enabled":true,"capabilities":{"supports":{"tool_calls":true}}},
                {"id":"claude-sonnet-4.5","model_picker_enabled":true,"policy":{"state":"disabled"},"capabilities":{"supports":{"tool_calls":true}}},
                {"id":"gpt-5.4","model_picker_enabled":false,"policy":{"state":"enabled"},"capabilities":{"supports":{"tool_calls":true}}}
            ]),
            true,
        );
        assert_eq!(picker.available_model_ids, [ModelId::new("gpt-4.1")]);

        let fallback_data = serde_json::json!([
            {"id":"gpt-4.1","model_picker_enabled":false,"policy":{"state":"enabled"},"capabilities":{"supports":{"tool_calls":true}}},
            {"id":"claude-sonnet-4.5","model_picker_enabled":false,"policy":{"state":"disabled"},"capabilities":{"supports":{"tool_calls":true}}},
            {"id":"gpt-5.4","model_picker_enabled":false,"policy":{"state":"enabled"},"capabilities":{"supports":{"tool_calls":false}}}
        ]);
        assert_eq!(
            parse(fallback_data.clone(), true).available_model_ids,
            [ModelId::new("gpt-4.1")]
        );
        assert!(parse(fallback_data, false).available_model_ids.is_empty());

        let policies = parse(
            serde_json::json!([
                {"id":"gpt-4.1","model_picker_enabled":true,"policy":{"state":"enabled"},"capabilities":{"supports":{"tool_calls":true}}},
                {"id":"claude-sonnet-4.5","model_picker_enabled":true,"policy":{"state":"unconfigured"},"capabilities":{"supports":{"tool_calls":true}}},
                {"id":"remote-only-model","model_picker_enabled":true,"policy":{"state":"unconfigured"},"capabilities":{"supports":{"tool_calls":true}}},
                {"id":"gpt-5.4","model_picker_enabled":true,"policy":{"state":"unconfigured"},"capabilities":{"supports":{"tool_calls":false}}}
            ]),
            true,
        );
        assert_eq!(
            policies.policy_model_ids,
            [ModelId::new("claude-sonnet-4.5")]
        );
    }

    #[test]
    fn github_copilot_request_deadline_covers_stalled_transport_and_body_send_and_local() {
        // Architecture v2 part 2 §6 and §9.2; Pi basis:
        // packages/ai/src/auth/oauth/github-copilot.ts:135-165 applies a
        // five-second signal to each `/models` and policy fetch and a
        // five-second overall retry-budget signal. A short deadline exercises
        // the same portable watchdog without making this hermetic test slow.
        let models = bearer_get(
            Url::parse("https://api.individual.githubcopilot.com/models").unwrap(),
            "fixture-token",
            true,
        )
        .unwrap();
        let policy = policy_request(
            Url::parse("https://api.individual.githubcopilot.com/models/test/policy").unwrap(),
            "fixture-token",
        )
        .unwrap();
        assert_eq!(models.timeout, Some(COPILOT_REQUEST_TIMEOUT));
        assert_eq!(policy.timeout, Some(COPILOT_REQUEST_TIMEOUT));

        for stall in [StallAt::Establishment, StallAt::Body] {
            let error = futures_executor::block_on(execute_copilot_send(
                &StalledTransport(stall),
                short_deadline_request(),
                CancellationToken::new(),
                None,
            ))
            .unwrap_err();
            assert_eq!(error.code(), "github_copilot_request_timeout");

            let error = futures_executor::block_on(execute_copilot_local(
                &StalledTransport(stall),
                short_deadline_request(),
                CancellationToken::new(),
                None,
            ))
            .unwrap_err();
            assert_eq!(error.code(), "github_copilot_request_timeout");
        }
    }
}
