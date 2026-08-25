//! Dependency-light construction helpers used by provider leaf crates.

use futures_util::{FutureExt, StreamExt};
use http::{HeaderMap, HeaderValue, Method, header};
use pi_ai::{
    ApiId, AuthError, AuthInteraction, AuthResolver, CancellationToken, EnvironmentApiKeyAuth,
    HttpRequest, HttpTransport, LocalAuthInteraction, LocalAuthResolver, LocalBoxFuture,
    LocalHttpTransport, LocalOAuthAuth, LocalProviderAuthResolver, LocalProviderRegistration,
    LocalResolveAuthRequest, OAuthAuth, ProviderAuthResolver, ProviderRegistration,
    ProviderRegistrationError, ResolveAuthRequest, ResolvedAuth, SendBoxFuture,
};
use std::collections::BTreeMap;
use std::fmt;
use std::rc::Rc;
use std::sync::Arc;
use url::Url;

/// Send transport input shared by ordinary leaf-provider constructors.
#[derive(Clone)]
pub struct ProviderInputs {
    /// Raw executor-neutral HTTP transport.
    pub http: Arc<dyn HttpTransport>,
    /// Host-injected environment values required while constructing flows
    /// whose endpoint is selected before request-scoped auth resolution.
    pub environment: BTreeMap<String, String>,
}

/// Local-executor transport input shared by ordinary leaf constructors.
#[derive(Clone)]
pub struct LocalProviderInputs {
    /// Raw local HTTP transport.
    pub http: Rc<dyn LocalHttpTransport>,
    /// Local host-injected construction-time environment values.
    pub environment: BTreeMap<String, String>,
}

/// Failure while constructing one leaf-provider registration.
#[derive(Debug)]
pub enum ProviderBuildError {
    /// Pinned provider-owned catalog data could not be represented.
    Catalog(String),
    /// Provider construction configuration was invalid.
    Configuration(String),
    /// Registration omitted or contradicted one of its API implementations.
    Registration(ProviderRegistrationError),
}

impl ProviderBuildError {
    /// Converts a provider/API catalog error without erasing its public text.
    pub fn catalog(error: impl fmt::Display) -> Self {
        Self::Catalog(error.to_string())
    }

    /// Converts a construction-time configuration failure.
    pub fn configuration(error: impl fmt::Display) -> Self {
        Self::Configuration(error.to_string())
    }
}

impl fmt::Display for ProviderBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Catalog(error) => write!(formatter, "catalog error: {error}"),
            Self::Configuration(error) => write!(formatter, "configuration error: {error}"),
            Self::Registration(error) => write!(formatter, "registration error: {error}"),
        }
    }
}

impl std::error::Error for ProviderBuildError {}

/// Builds a Send registration from leaf-owned metadata and implementations.
pub fn build_provider(
    id: &str,
    display_name: &str,
    models: Vec<pi_ai::ModelDescriptor>,
    auth: Arc<dyn AuthResolver>,
    apis: impl IntoIterator<Item = (ApiId, Arc<dyn pi_ai::ChatApi>)>,
) -> Result<ProviderRegistration, ProviderBuildError> {
    let mut builder = ProviderRegistration::builder(id)
        .display_name(display_name)
        .auth(auth)
        .models(models);
    for (api, implementation) in apis {
        builder = builder.api(api, implementation);
    }
    builder.build().map_err(ProviderBuildError::Registration)
}

/// Builds a local registration from leaf-owned metadata and implementations.
pub fn build_local_provider(
    id: &str,
    display_name: &str,
    models: Vec<pi_ai::ModelDescriptor>,
    auth: Rc<dyn LocalAuthResolver>,
    apis: impl IntoIterator<Item = (ApiId, Rc<dyn pi_ai::LocalChatApi>)>,
) -> Result<LocalProviderRegistration, ProviderBuildError> {
    let mut builder = LocalProviderRegistration::builder(id)
        .display_name(display_name)
        .auth(auth)
        .models(models);
    for (api, implementation) in apis {
        builder = builder.api(api, implementation);
    }
    builder.build().map_err(ProviderBuildError::Registration)
}

/// Standard API-key/OAuth resolver whose provider request auth is Bearer.
pub fn bearer_auth(
    method_name: &'static str,
    environment: &'static str,
    oauth: Option<Arc<dyn OAuthAuth>>,
) -> Arc<dyn AuthResolver> {
    Arc::new(BearerAuth {
        inner: ProviderAuthResolver::new(
            Some(Arc::new(EnvironmentApiKeyAuth::new(
                method_name,
                [environment],
            ))),
            oauth,
        ),
    })
}

/// Local counterpart to [`bearer_auth`].
pub fn local_bearer_auth(
    method_name: &'static str,
    environment: &'static str,
    oauth: Option<Rc<dyn LocalOAuthAuth>>,
) -> Rc<dyn LocalAuthResolver> {
    Rc::new(LocalBearerAuth {
        inner: LocalProviderAuthResolver::new(
            Some(Rc::new(EnvironmentApiKeyAuth::new(
                method_name,
                [environment],
            ))),
            oauth,
        ),
    })
}

/// Standard API-key/OAuth resolver that materializes API-key credentials in
/// the header form owned by the selected API family.
///
/// Mixed providers cannot decide this at registration time: Anthropic
/// Messages uses `x-api-key`, Google uses `x-goog-api-key`, and OpenAI-style
/// families use bearer authorization. The selected catalog model remains
/// available to the auth seam, so conversion is intentionally delayed until
/// each request is resolved.
pub fn family_auth(
    method_name: &'static str,
    environment: &'static str,
    oauth: Option<Arc<dyn OAuthAuth>>,
) -> Arc<dyn AuthResolver> {
    Arc::new(FamilyAuth {
        inner: ProviderAuthResolver::new(
            Some(Arc::new(EnvironmentApiKeyAuth::new(
                method_name,
                [environment],
            ))),
            oauth,
        ),
    })
}

/// Local counterpart to [`family_auth`].
pub fn local_family_auth(
    method_name: &'static str,
    environment: &'static str,
    oauth: Option<Rc<dyn LocalOAuthAuth>>,
) -> Rc<dyn LocalAuthResolver> {
    Rc::new(LocalFamilyAuth {
        inner: LocalProviderAuthResolver::new(
            Some(Rc::new(EnvironmentApiKeyAuth::new(
                method_name,
                [environment],
            ))),
            oauth,
        ),
    })
}

struct FamilyAuth {
    inner: ProviderAuthResolver,
}

impl AuthResolver for FamilyAuth {
    fn resolve(
        &self,
        request: ResolveAuthRequest,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>> {
        let api = request.model.as_ref().map(|model| model.api.api_id());
        Box::pin(async move {
            let Some(mut resolved) = self.inner.resolve(request, cancellation).await? else {
                return Ok(None);
            };
            move_api_key_to_family_header(&mut resolved, api.as_ref())?;
            Ok(Some(resolved))
        })
    }

    fn login(
        &self,
        interaction: Arc<dyn AuthInteraction>,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<pi_ai::Credential, AuthError>> {
        self.inner.login(interaction, cancellation)
    }

    fn logout(&self, cancellation: CancellationToken) -> SendBoxFuture<'_, Result<(), AuthError>> {
        self.inner.logout(cancellation)
    }
}

struct LocalFamilyAuth {
    inner: LocalProviderAuthResolver,
}

impl LocalAuthResolver for LocalFamilyAuth {
    fn resolve(
        &self,
        request: LocalResolveAuthRequest,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>> {
        let api = request.model.as_ref().map(|model| model.api.api_id());
        Box::pin(async move {
            let Some(mut resolved) = self.inner.resolve(request, cancellation).await? else {
                return Ok(None);
            };
            move_api_key_to_family_header(&mut resolved, api.as_ref())?;
            Ok(Some(resolved))
        })
    }

    fn login(
        &self,
        interaction: Rc<dyn LocalAuthInteraction>,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<pi_ai::Credential, AuthError>> {
        self.inner.login(interaction, cancellation)
    }

    fn logout(&self, cancellation: CancellationToken) -> LocalBoxFuture<'_, Result<(), AuthError>> {
        self.inner.logout(cancellation)
    }
}

struct BearerAuth {
    inner: ProviderAuthResolver,
}

impl AuthResolver for BearerAuth {
    fn resolve(
        &self,
        request: ResolveAuthRequest,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>> {
        Box::pin(async move {
            let Some(mut resolved) = self.inner.resolve(request, cancellation).await? else {
                return Ok(None);
            };
            move_api_key_to_bearer(&mut resolved)?;
            Ok(Some(resolved))
        })
    }

    fn login(
        &self,
        interaction: Arc<dyn AuthInteraction>,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<pi_ai::Credential, AuthError>> {
        self.inner.login(interaction, cancellation)
    }
}

struct LocalBearerAuth {
    inner: LocalProviderAuthResolver,
}

impl LocalAuthResolver for LocalBearerAuth {
    fn resolve(
        &self,
        request: LocalResolveAuthRequest,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>> {
        Box::pin(async move {
            let Some(mut resolved) = self.inner.resolve(request, cancellation).await? else {
                return Ok(None);
            };
            move_api_key_to_bearer(&mut resolved)?;
            Ok(Some(resolved))
        })
    }

    fn login(
        &self,
        interaction: Rc<dyn LocalAuthInteraction>,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<pi_ai::Credential, AuthError>> {
        self.inner.login(interaction, cancellation)
    }
}

fn move_api_key_to_bearer(resolved: &mut ResolvedAuth) -> Result<(), AuthError> {
    let Some(key) = resolved.api_key.take() else {
        return Ok(());
    };
    let value = HeaderValue::from_str(&format!("Bearer {}", key.expose_secret()))
        .map_err(|_| AuthError::new("invalid_api_key", "API key is not a valid header"))?;
    resolved.headers.insert(header::AUTHORIZATION, value);
    Ok(())
}

fn move_api_key_to_family_header(
    resolved: &mut ResolvedAuth,
    api: Option<&ApiId>,
) -> Result<(), AuthError> {
    match api.map(ApiId::as_str) {
        Some("anthropic-messages") => move_api_key_to_named_header(resolved, "x-api-key", true),
        Some("google-generative-ai" | "google-vertex") => {
            move_api_key_to_named_header(resolved, "x-goog-api-key", false)
        }
        _ => move_api_key_to_bearer(resolved),
    }
}

fn move_api_key_to_named_header(
    resolved: &mut ResolvedAuth,
    name: &'static str,
    consume: bool,
) -> Result<(), AuthError> {
    let Some(key) = resolved.api_key.as_ref() else {
        return Ok(());
    };
    let value = HeaderValue::from_str(key.expose_secret())
        .map_err(|_| AuthError::new("invalid_api_key", "API key is not a valid header"))?;
    resolved.headers.insert(name, value);
    if consume {
        resolved.api_key = None;
    }
    Ok(())
}

/// Creates one POST request with an exact URL-encoded form body.
pub fn form_post(url: &str, fields: &[(&str, &str)]) -> Result<HttpRequest, AuthError> {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (name, value) in fields {
        serializer.append_pair(name, value);
    }
    request_with_body(
        url,
        serializer.finish().into_bytes(),
        "application/x-www-form-urlencoded",
    )
}

/// Creates one POST request with an exact JSON body.
pub fn json_post(url: &str, body: Vec<u8>) -> Result<HttpRequest, AuthError> {
    request_with_body(url, body, "application/json")
}

fn request_with_body(
    url: &str,
    body: Vec<u8>,
    content_type: &'static str,
) -> Result<HttpRequest, AuthError> {
    let mut headers = HeaderMap::new();
    headers.insert(header::ACCEPT, HeaderValue::from_static("application/json"));
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    Ok(HttpRequest {
        method: Method::POST,
        url: Url::parse(url)
            .map_err(|error| AuthError::new("oauth_url", format!("invalid OAuth URL: {error}")))?,
        headers,
        auth_headers: HeaderMap::new(),
        session_id: None,
        body,
        timeout: Some(std::time::Duration::from_secs(30)),
        transport: None,
        websocket_connect_timeout: None,
        attempt: 0,
    })
}

/// Executes a Send OAuth request and collects its body without a runtime SDK.
pub async fn execute_send(
    transport: &dyn HttpTransport,
    request: HttpRequest,
    cancellation: CancellationToken,
) -> Result<(u16, HeaderMap, Vec<u8>), AuthError> {
    let response = transport
        .execute(request, cancellation.clone())
        .await
        .map_err(|error| {
            AuthError::new("oauth_transport", format!("OAuth request failed: {error}"))
        })?;
    let status = response.status;
    let headers = response.headers;
    let mut body = response.body;
    let mut bytes = Vec::new();
    loop {
        cancellation.check().map_err(|_| AuthError::Cancelled)?;
        let next = body.next().fuse();
        let cancelled = cancellation.cancelled().fuse();
        futures_util::pin_mut!(next, cancelled);
        let chunk = futures_util::select_biased! {
            _ = cancelled => return Err(AuthError::Cancelled),
            chunk = next => chunk,
        };
        let Some(chunk) = chunk else { break };
        bytes.extend(chunk.map_err(|error| {
            AuthError::new("oauth_transport", format!("OAuth response failed: {error}"))
        })?);
    }
    Ok((status, headers, bytes))
}

/// Executes a local OAuth request and collects its body.
pub async fn execute_local(
    transport: &dyn LocalHttpTransport,
    request: HttpRequest,
    cancellation: CancellationToken,
) -> Result<(u16, HeaderMap, Vec<u8>), AuthError> {
    let response = transport
        .execute(request, cancellation.clone())
        .await
        .map_err(|error| {
            AuthError::new("oauth_transport", format!("OAuth request failed: {error}"))
        })?;
    let status = response.status;
    let headers = response.headers;
    let mut body = response.body;
    let mut bytes = Vec::new();
    loop {
        cancellation.check().map_err(|_| AuthError::Cancelled)?;
        let next = body.next().fuse();
        let cancelled = cancellation.cancelled().fuse();
        futures_util::pin_mut!(next, cancelled);
        let chunk = futures_util::select_biased! {
            _ = cancelled => return Err(AuthError::Cancelled),
            chunk = next => chunk,
        };
        let Some(chunk) = chunk else { break };
        bytes.extend(chunk.map_err(|error| {
            AuthError::new("oauth_transport", format!("OAuth response failed: {error}"))
        })?);
    }
    Ok((status, headers, bytes))
}

/// Defines an ordinary static provider in the invoking leaf crate.
///
/// All provider identity, catalog data, auth labels, environment names, and
/// API-family dependencies remain in that leaf; this macro only removes the
/// mechanical Send/Local builder duplication.
#[macro_export]
macro_rules! define_static_provider {
    (
        id: $id:literal,
        name: $name:literal,
        auth_name: $auth_name:literal,
        env: $env:literal,
        catalog: $catalog:expr,
        send_apis: [
            ($api:literal, $($send:ident)::+ ($send_input:ident . http . clone()))
            $(, ($more_api:literal, $($more_send:ident)::+ ($more_send_input:ident . http . clone())))*
            $(,)?
        ],
        local_apis: [
            ($local_api:literal, $($local:ident)::+ ($local_input:ident . http . clone()))
            $(, ($more_local_api:literal, $($more_local:ident)::+ ($more_local_input:ident . http . clone())))*
            $(,)?
        ]
    ) => {
        /// Returns this leaf's pinned provider-owned catalog.
        pub fn models() -> Result<Vec<pi_ai::ModelDescriptor>, $crate::ProviderBuildError> {
            ($catalog).map_err($crate::ProviderBuildError::catalog)
        }

        /// Builds this leaf's Send provider registration.
        pub fn provider(
            $send_input: $crate::ProviderInputs,
        ) -> Result<pi_ai::ProviderRegistration, $crate::ProviderBuildError> {
            let apis: Vec<(pi_ai::ApiId, std::sync::Arc<dyn pi_ai::ChatApi>)> = vec![
                (pi_ai::ApiId::new($api), $($send)::+($send_input.http.clone())),
                $((pi_ai::ApiId::new($more_api), $($more_send)::+($more_send_input.http.clone()))),*
            ];
            $crate::build_provider(
                $id,
                $name,
                models()?,
                $crate::family_auth($auth_name, $env, None),
                apis,
            )
        }

        /// Builds this leaf's local-executor provider registration.
        pub fn local_provider(
            $local_input: $crate::LocalProviderInputs,
        ) -> Result<pi_ai::LocalProviderRegistration, $crate::ProviderBuildError> {
            let apis: Vec<(pi_ai::ApiId, std::rc::Rc<dyn pi_ai::LocalChatApi>)> = vec![
                (pi_ai::ApiId::new($local_api), $($local)::+($local_input.http.clone())),
                $((pi_ai::ApiId::new($more_local_api), $($more_local)::+($more_local_input.http.clone()))),*
            ];
            $crate::build_local_provider(
                $id,
                $name,
                models()?,
                $crate::local_family_auth($auth_name, $env, None),
                apis,
            )
        }
    };
}
