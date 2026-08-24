use crate::{PiAuthSessionEvent, PiAuthSessionEventStatus, PiFfiError};
use futures_util::future::{Either, select};
use pi_ai::{
    AuthAnswer, AuthChallengeId, AuthEvent, AuthHostCapabilities, AuthInteraction,
    AuthInteractionError, AuthPrompt, CancellationToken, RedirectArrival, RedirectReceiver,
    RedirectReceiverRequest, RedirectStrategy, SendBoxFuture, Timestamp,
    redirect_strategy_supported,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::sync::oneshot;
use url::Url;

/// Bounded host-facing auth queue. A slow host backpressures provider login;
/// challenge and terminal messages are never discarded.
const AUTH_SESSION_QUEUE_CAPACITY: usize = 16;

pub(crate) struct AuthSessionCore {
    interaction: Arc<SessionInteraction>,
    messages: Mutex<Receiver<SessionMessage>>,
    cancellation: CancellationToken,
    terminal_seen: AtomicBool,
}

impl AuthSessionCore {
    pub(crate) fn new(
        capabilities: AuthHostCapabilities,
        requested_auth_type: String,
    ) -> (
        Arc<Self>,
        Arc<SessionInteraction>,
        SyncSender<SessionMessage>,
    ) {
        let (sender, receiver) = sync_channel(AUTH_SESSION_QUEUE_CAPACITY);
        let interaction = Arc::new(SessionInteraction {
            capabilities,
            requested_auth_type,
            sender: sender.clone(),
            registry: Mutex::new(ChallengeRegistry::default()),
            next_challenge: AtomicU64::new(1),
        });
        let core = Arc::new(Self {
            interaction: Arc::clone(&interaction),
            messages: Mutex::new(receiver),
            cancellation: CancellationToken::new(),
            terminal_seen: AtomicBool::new(false),
        });
        (core, interaction, sender)
    }

    pub(crate) fn cancellation(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    pub(crate) fn next(&self) -> Result<PiAuthSessionEvent, PiFfiError> {
        if self.terminal_seen.load(Ordering::Acquire) {
            return Err(PiFfiError::SessionClosed);
        }
        let message = lock_unpoisoned(&self.messages)
            .recv()
            .map_err(|_| PiFfiError::SessionClosed)?;
        let event = message.into_public();
        if event.status != PiAuthSessionEventStatus::Challenge {
            self.terminal_seen.store(true, Ordering::Release);
        }
        Ok(event)
    }

    pub(crate) fn respond(
        &self,
        challenge_id: &str,
        response_json: &str,
    ) -> Result<RespondDisposition, PiFfiError> {
        let response = serde_json::from_str::<WireResponse>(response_json).map_err(|error| {
            PiFfiError::InvalidJson {
                message: format!("invalid authentication response JSON: {error}"),
            }
        })?;
        self.interaction.respond(challenge_id, response)
    }

    pub(crate) fn cancel(&self) {
        self.cancellation.cancel();
        self.interaction.cancel_open_challenges();
    }
}

pub(crate) enum SessionMessage {
    Challenge(String),
    Completed(String),
    Failed(String),
    Cancelled(String),
}

impl SessionMessage {
    fn into_public(self) -> PiAuthSessionEvent {
        match self {
            Self::Challenge(json) => PiAuthSessionEvent {
                status: PiAuthSessionEventStatus::Challenge,
                json,
            },
            Self::Completed(json) => PiAuthSessionEvent {
                status: PiAuthSessionEventStatus::Completed,
                json,
            },
            Self::Failed(json) => PiAuthSessionEvent {
                status: PiAuthSessionEventStatus::Failed,
                json,
            },
            Self::Cancelled(json) => PiAuthSessionEvent {
                status: PiAuthSessionEventStatus::Cancelled,
                json,
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RespondDisposition {
    Accepted,
    Superseded,
}

pub(crate) fn completed_message(provider_id: &str) -> SessionMessage {
    SessionMessage::Completed(
        json!({
            "schemaVersion": 1,
            "type": "completed",
            "providerId": provider_id,
        })
        .to_string(),
    )
}

pub(crate) fn failed_message(code: &str, message: &str) -> SessionMessage {
    SessionMessage::Failed(
        json!({
            "schemaVersion": 1,
            "type": "failed",
            "error": {
                "code": code,
                "message": message,
            },
        })
        .to_string(),
    )
}

pub(crate) fn cancelled_message() -> SessionMessage {
    SessionMessage::Cancelled(
        json!({
            "schemaVersion": 1,
            "type": "cancelled",
            "error": {
                "code": "cancelled",
                "message": "authentication cancelled",
            },
        })
        .to_string(),
    )
}

pub(crate) struct SessionInteraction {
    capabilities: AuthHostCapabilities,
    requested_auth_type: String,
    sender: SyncSender<SessionMessage>,
    registry: Mutex<ChallengeRegistry>,
    next_challenge: AtomicU64,
}

impl SessionInteraction {
    fn next_challenge_id(&self) -> AuthChallengeId {
        AuthChallengeId::new(format!(
            "challenge-{}",
            self.next_challenge.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn emit(&self, value: Value) -> Result<(), AuthInteractionError> {
        self.sender
            .send(SessionMessage::Challenge(value.to_string()))
            .map_err(|_| AuthInteractionError::Cancelled)
    }

    fn register_waiter(
        &self,
        challenge_id: AuthChallengeId,
        expected: ExpectedResponse,
    ) -> Result<ChallengeReceiver, AuthInteractionError> {
        let (sender, receiver) = oneshot::channel();
        let mut registry = lock_unpoisoned(&self.registry);
        if registry.closed.contains(&challenge_id) {
            return Err(AuthInteractionError::ChallengeSuperseded { challenge_id });
        }
        registry.known.insert(challenge_id.clone());
        registry
            .open
            .entry(challenge_id)
            .or_default()
            .push(ChallengeWaiter { expected, sender });
        Ok(receiver)
    }

    fn prepare_manual_waiter(
        &self,
        challenge_id: AuthChallengeId,
    ) -> Result<(), AuthInteractionError> {
        if !self.capabilities.manual_paste {
            return Ok(());
        }
        let receiver = self.register_waiter(challenge_id.clone(), ExpectedResponse::ManualCode)?;
        let replaced = lock_unpoisoned(&self.registry)
            .prepared_manual
            .insert(challenge_id, receiver);
        if replaced.is_some() {
            return Err(AuthInteractionError::Failed {
                code: "duplicate_manual_challenge".into(),
                message: "authentication challenge prepared manual input more than once".into(),
            });
        }
        Ok(())
    }

    fn take_prepared_manual(&self, challenge_id: &AuthChallengeId) -> Option<ChallengeReceiver> {
        lock_unpoisoned(&self.registry)
            .prepared_manual
            .remove(challenge_id)
    }

    fn set_resolved_redirect(
        &self,
        challenge_id: AuthChallengeId,
        redirect: ResolvedRedirect,
    ) -> Result<(), AuthInteractionError> {
        let replaced = lock_unpoisoned(&self.registry)
            .resolved_redirects
            .insert(challenge_id, redirect);
        if replaced.is_some() {
            return Err(AuthInteractionError::Failed {
                code: "duplicate_redirect_challenge".into(),
                message: "authentication challenge resolved more than one redirect".into(),
            });
        }
        Ok(())
    }

    fn resolved_redirect(&self, challenge_id: &AuthChallengeId) -> Option<ResolvedRedirect> {
        lock_unpoisoned(&self.registry)
            .resolved_redirects
            .get(challenge_id)
            .cloned()
    }

    fn respond(
        &self,
        challenge_id: &str,
        response: WireResponse,
    ) -> Result<RespondDisposition, PiFfiError> {
        let challenge_id = AuthChallengeId::new(challenge_id);
        let mut registry = lock_unpoisoned(&self.registry);
        if registry.closed.contains(&challenge_id) {
            return Ok(RespondDisposition::Superseded);
        }
        let Some(waiters) = registry.open.get_mut(&challenge_id) else {
            return Err(PiFfiError::UnknownChallenge {
                challenge_id: challenge_id.into_inner(),
            });
        };
        let Some((winner_index, session_response)) =
            waiters.iter().enumerate().find_map(|(index, waiter)| {
                waiter
                    .expected
                    .accept(&response)
                    .map(|response| (index, response))
            })
        else {
            return Err(PiFfiError::InvalidAuthResponse {
                challenge_id: challenge_id.into_inner(),
            });
        };

        let winner = waiters.remove(winner_index);
        let remove_open_entry = waiters.is_empty();
        if remove_open_entry {
            registry.open.remove(&challenge_id);
        }
        drop(registry);

        if winner.sender.send(Ok(session_response)).is_err() {
            return Ok(RespondDisposition::Superseded);
        }

        // Delivering a structurally matching response only starts provider
        // validation. Keep every competing waiter open until the provider
        // accepts a semantic winner and the session calls
        // `finish_open_challenges`; an invalid manual code must not cancel a
        // still-pending redirect callback.
        Ok(RespondDisposition::Accepted)
    }

    pub(crate) fn finish_open_challenges(&self) {
        let mut registry = lock_unpoisoned(&self.registry);
        let open = std::mem::take(&mut registry.open);
        registry.prepared_manual.clear();
        registry.resolved_redirects.clear();
        let known = registry.known.clone();
        registry.closed.extend(known);
        drop(registry);
        for (challenge_id, waiters) in open {
            for waiter in waiters {
                let _ = waiter
                    .sender
                    .send(Err(AuthInteractionError::ChallengeSuperseded {
                        challenge_id: challenge_id.clone(),
                    }));
            }
        }
    }

    fn cancel_open_challenges(&self) {
        let mut registry = lock_unpoisoned(&self.registry);
        let open = std::mem::take(&mut registry.open);
        registry.prepared_manual.clear();
        registry.resolved_redirects.clear();
        let known = registry.known.clone();
        registry.closed.extend(known);
        drop(registry);
        for waiters in open.into_values() {
            for waiter in waiters {
                let _ = waiter.sender.send(Err(AuthInteractionError::Cancelled));
            }
        }
    }

    async fn await_response(
        receiver: oneshot::Receiver<Result<SessionResponse, AuthInteractionError>>,
        cancellation: CancellationToken,
    ) -> Result<SessionResponse, AuthInteractionError> {
        let response = Box::pin(receiver);
        let cancelled = Box::pin(cancellation.cancelled());
        match select(response, cancelled).await {
            Either::Left((Ok(result), _)) => result,
            Either::Left((Err(_), _)) | Either::Right(((), _)) => {
                Err(AuthInteractionError::Cancelled)
            }
        }
    }

    fn choose_redirect(
        &self,
        preferred: &[RedirectStrategy],
    ) -> Result<RedirectStrategy, AuthInteractionError> {
        preferred
            .iter()
            .find(|strategy| redirect_strategy_supported(strategy, &self.capabilities))
            .cloned()
            .ok_or_else(|| AuthInteractionError::Unsupported {
                message: "the host does not support any provider redirect strategy".into(),
            })
    }
}

impl AuthInteraction for SessionInteraction {
    fn capabilities(&self) -> AuthHostCapabilities {
        self.capabilities.clone()
    }

    fn prompt(
        &self,
        prompt: AuthPrompt,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<AuthAnswer, AuthInteractionError>> {
        Box::pin(async move {
            if let AuthPrompt::Select { options, .. } = &prompt
                && options
                    .iter()
                    .any(|option| option.id == self.requested_auth_type)
            {
                return Ok(AuthAnswer::Selected(self.requested_auth_type.clone()));
            }

            let (challenge_id, expected, value) = match prompt {
                AuthPrompt::Text {
                    message,
                    placeholder,
                } => {
                    let challenge_id = self.next_challenge_id();
                    (
                        challenge_id.clone(),
                        ExpectedResponse::Text,
                        json!({
                            "schemaVersion": 1,
                            "id": challenge_id,
                            "type": "text",
                            "message": message,
                            "placeholder": placeholder,
                        }),
                    )
                }
                AuthPrompt::Secret {
                    message,
                    placeholder,
                } => {
                    let challenge_id = self.next_challenge_id();
                    (
                        challenge_id.clone(),
                        ExpectedResponse::Secret,
                        json!({
                            "schemaVersion": 1,
                            "id": challenge_id,
                            "type": "secret",
                            "message": message,
                            "placeholder": placeholder,
                        }),
                    )
                }
                AuthPrompt::Select { message, options } => {
                    let challenge_id = self.next_challenge_id();
                    let allowed = options.iter().map(|option| option.id.clone()).collect();
                    (
                        challenge_id.clone(),
                        ExpectedResponse::Select { allowed },
                        json!({
                            "schemaVersion": 1,
                            "id": challenge_id,
                            "type": "select",
                            "message": message,
                            "options": options,
                        }),
                    )
                }
                AuthPrompt::ManualCode {
                    message,
                    placeholder,
                    challenge_id,
                } => {
                    if !self.capabilities.manual_paste {
                        return Err(AuthInteractionError::Unsupported {
                            message: "the host does not accept manually pasted authorization codes"
                                .into(),
                        });
                    }
                    if let Some(response) = self.take_prepared_manual(&challenge_id) {
                        return match Self::await_response(response, cancellation).await? {
                            SessionResponse::ManualCode(value) => Ok(AuthAnswer::Text(value)),
                            SessionResponse::RedirectArrived(arrival) => {
                                Ok(AuthAnswer::Text(arrival.url.to_string()))
                            }
                            _ => Err(AuthInteractionError::Failed {
                                code: "invalid_auth_response".into(),
                                message: "manual authorization returned an incompatible response"
                                    .into(),
                            }),
                        };
                    }
                    (
                        challenge_id.clone(),
                        ExpectedResponse::ManualCode,
                        json!({
                            "schemaVersion": 1,
                            "id": challenge_id,
                            "type": "manual_code",
                            "message": message,
                            "placeholder": placeholder,
                        }),
                    )
                }
            };
            let response = self.register_waiter(challenge_id, expected)?;
            self.emit(value)?;
            match Self::await_response(response, cancellation).await? {
                SessionResponse::Text(value) | SessionResponse::ManualCode(value) => {
                    Ok(AuthAnswer::Text(value))
                }
                SessionResponse::Selected(value) => Ok(AuthAnswer::Selected(value)),
                SessionResponse::RedirectArrived(arrival) => {
                    Ok(AuthAnswer::Text(arrival.url.to_string()))
                }
                SessionResponse::RedirectReady(_) => Err(AuthInteractionError::Failed {
                    code: "invalid_auth_response".into(),
                    message: "redirect readiness cannot answer an authentication prompt".into(),
                }),
            }
        })
    }

    fn notify(&self, event: AuthEvent) -> Result<(), AuthInteractionError> {
        match event {
            AuthEvent::Info { message, links } => self.emit(json!({
                "schemaVersion": 1,
                "id": self.next_challenge_id(),
                "type": "info",
                "message": message,
                "links": links,
            })),
            AuthEvent::OpenUrl {
                challenge_id,
                url,
                instructions,
            } => {
                let redirect = self.resolved_redirect(&challenge_id).ok_or_else(|| {
                    AuthInteractionError::Failed {
                        code: "missing_redirect_receiver".into(),
                        message: "authorization URL was emitted before its redirect was resolved"
                            .into(),
                    }
                })?;
                let also_accepts_manual_code = lock_unpoisoned(&self.registry)
                    .prepared_manual
                    .contains_key(&challenge_id);
                self.emit(json!({
                    "schemaVersion": 1,
                    "id": challenge_id,
                    "type": "open_url",
                    "url": url,
                    "redirect": {
                        "strategy": redirect_strategy_name(&redirect.strategy),
                        "uri": redirect.uri,
                    },
                    "instructions": instructions,
                    "alsoAcceptsManualCode": also_accepts_manual_code,
                }))
            }
            AuthEvent::DeviceCode {
                challenge_id,
                user_code,
                verification_uri,
                interval,
                expires_in,
            } => {
                lock_unpoisoned(&self.registry)
                    .known
                    .insert(challenge_id.clone());
                self.emit(json!({
                    "schemaVersion": 1,
                    "id": challenge_id,
                    "type": "device_code",
                    "userCode": user_code,
                    "verificationUri": verification_uri,
                    "intervalSeconds": duration_seconds(interval),
                    "expiresInSeconds": duration_seconds(expires_in),
                }))
            }
            AuthEvent::Progress { message } => self.emit(json!({
                "schemaVersion": 1,
                "id": self.next_challenge_id(),
                "type": "progress",
                "message": message,
            })),
        }
    }

    fn create_redirect_receiver(
        &self,
        request: RedirectReceiverRequest,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Box<dyn RedirectReceiver>, AuthInteractionError>> {
        Box::pin(async move {
            let strategy = self.choose_redirect(&request.preferred)?;
            let challenge_id = request.challenge_id;
            let uri = match &strategy {
                RedirectStrategy::EphemeralLoopback { .. } => {
                    let receiver = self
                        .register_waiter(challenge_id.clone(), ExpectedResponse::RedirectReady)?;
                    self.emit(json!({
                        "schemaVersion": 1,
                        "id": challenge_id,
                        "type": "redirect_receiver",
                        "strategy": redirect_strategy_json(&strategy),
                    }))?;
                    match Self::await_response(receiver, cancellation.clone()).await? {
                        SessionResponse::RedirectReady(uri) => uri,
                        _ => {
                            return Err(AuthInteractionError::Failed {
                                code: "invalid_redirect_ready".into(),
                                message: "host did not provide a redirect URI".into(),
                            });
                        }
                    }
                }
                _ => redirect_uri_for_strategy(&strategy)?,
            };
            let receiver =
                self.register_waiter(challenge_id.clone(), ExpectedResponse::RedirectArrival)?;
            self.prepare_manual_waiter(challenge_id.clone())?;
            self.set_resolved_redirect(
                challenge_id,
                ResolvedRedirect {
                    strategy,
                    uri: uri.clone(),
                },
            )?;
            Ok(Box::new(SessionRedirectReceiver { uri, receiver }) as Box<dyn RedirectReceiver>)
        })
    }
}

type ChallengeReceiver = oneshot::Receiver<Result<SessionResponse, AuthInteractionError>>;

#[derive(Clone)]
struct ResolvedRedirect {
    strategy: RedirectStrategy,
    uri: Url,
}

#[derive(Default)]
struct ChallengeRegistry {
    known: HashSet<AuthChallengeId>,
    open: HashMap<AuthChallengeId, Vec<ChallengeWaiter>>,
    closed: HashSet<AuthChallengeId>,
    prepared_manual: HashMap<AuthChallengeId, ChallengeReceiver>,
    resolved_redirects: HashMap<AuthChallengeId, ResolvedRedirect>,
}

struct ChallengeWaiter {
    expected: ExpectedResponse,
    sender: oneshot::Sender<Result<SessionResponse, AuthInteractionError>>,
}

enum ExpectedResponse {
    Text,
    Secret,
    Select { allowed: HashSet<String> },
    ManualCode,
    RedirectReady,
    RedirectArrival,
}

impl ExpectedResponse {
    fn accept(&self, response: &WireResponse) -> Option<SessionResponse> {
        match (self, response) {
            (Self::Text | Self::Secret, WireResponse::Text { value }) => {
                Some(SessionResponse::Text(value.clone()))
            }
            (Self::Secret, WireResponse::Secret { value }) => {
                Some(SessionResponse::Text(value.clone()))
            }
            (Self::Select { allowed }, WireResponse::Selected { id }) if allowed.contains(id) => {
                Some(SessionResponse::Selected(id.clone()))
            }
            (Self::ManualCode, WireResponse::ManualCode { value }) => {
                Some(SessionResponse::ManualCode(value.clone()))
            }
            (Self::ManualCode, WireResponse::RedirectArrived { url, received_at }) => {
                parse_redirect_arrival(url, *received_at).map(SessionResponse::RedirectArrived)
            }
            (Self::RedirectReady, WireResponse::RedirectReady { uri }) => {
                Url::parse(uri).ok().map(SessionResponse::RedirectReady)
            }
            (Self::RedirectArrival, WireResponse::RedirectArrived { url, received_at }) => {
                parse_redirect_arrival(url, *received_at).map(SessionResponse::RedirectArrived)
            }
            _ => None,
        }
    }
}

enum SessionResponse {
    Text(String),
    Selected(String),
    ManualCode(String),
    RedirectReady(Url),
    RedirectArrived(RedirectArrival),
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum WireResponse {
    Text {
        value: String,
    },
    Secret {
        value: String,
    },
    Selected {
        id: String,
    },
    ManualCode {
        value: String,
    },
    RedirectReady {
        uri: String,
    },
    RedirectArrived {
        url: String,
        #[serde(default, rename = "receivedAt")]
        received_at: Option<i64>,
    },
}

struct SessionRedirectReceiver {
    uri: Url,
    receiver: oneshot::Receiver<Result<SessionResponse, AuthInteractionError>>,
}

impl RedirectReceiver for SessionRedirectReceiver {
    fn redirect_uri(&self) -> &Url {
        &self.uri
    }

    fn receive(
        self: Box<Self>,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'static, Result<RedirectArrival, AuthInteractionError>> {
        Box::pin(async move {
            match SessionInteraction::await_response(self.receiver, cancellation).await? {
                SessionResponse::RedirectArrived(arrival) => Ok(arrival),
                _ => Err(AuthInteractionError::Failed {
                    code: "invalid_redirect_response".into(),
                    message: "host response was not a redirect arrival".into(),
                }),
            }
        })
    }
}

fn parse_redirect_arrival(url: &str, received_at: Option<i64>) -> Option<RedirectArrival> {
    Some(RedirectArrival {
        url: Url::parse(url).ok()?,
        received_at: received_at
            .map(Timestamp::from_unix_millis)
            .unwrap_or_default(),
    })
}

fn duration_seconds(duration: Option<Duration>) -> Option<u64> {
    duration.map(|duration| duration.as_secs())
}

fn redirect_uri_for_strategy(strategy: &RedirectStrategy) -> Result<Url, AuthInteractionError> {
    let value = match strategy {
        RedirectStrategy::FixedLoopback { host, port, path } => {
            let host = display_url_host(*host);
            format!("http://{host}:{port}{}", normalized_path(path))
        }
        RedirectStrategy::CustomScheme { scheme, path } => {
            format!("{scheme}://callback{}", normalized_path(path))
        }
        RedirectStrategy::UniversalLink { origin, path } => origin
            .join(&normalized_path(path))
            .map_err(|error| AuthInteractionError::Failed {
                code: "invalid_redirect_uri".into(),
                message: error.to_string(),
            })?
            .to_string(),
        RedirectStrategy::ManualPaste => "urn:ietf:wg:oauth:2.0:oob".into(),
        RedirectStrategy::EphemeralLoopback { .. } => {
            return Err(AuthInteractionError::Unsupported {
                message: "ephemeral loopback needs a host-supplied redirect URI".into(),
            });
        }
    };
    Url::parse(&value).map_err(|error| AuthInteractionError::Failed {
        code: "invalid_redirect_uri".into(),
        message: error.to_string(),
    })
}

fn redirect_strategy_json(strategy: &RedirectStrategy) -> Value {
    match strategy {
        RedirectStrategy::FixedLoopback { host, port, path } => json!({
            "type": "fixed_loopback",
            "host": host,
            "port": port,
            "path": path,
        }),
        RedirectStrategy::EphemeralLoopback { host, path } => json!({
            "type": "ephemeral_loopback",
            "host": host,
            "path": path,
        }),
        RedirectStrategy::CustomScheme { scheme, path } => json!({
            "type": "custom_scheme",
            "scheme": scheme,
            "path": path,
        }),
        RedirectStrategy::UniversalLink { origin, path } => json!({
            "type": "universal_link",
            "origin": origin,
            "path": path,
        }),
        RedirectStrategy::ManualPaste => json!({
            "type": "manual_paste",
        }),
    }
}

fn redirect_strategy_name(strategy: &RedirectStrategy) -> &'static str {
    match strategy {
        RedirectStrategy::FixedLoopback { .. } => "fixed_loopback",
        RedirectStrategy::EphemeralLoopback { .. } => "ephemeral_loopback",
        RedirectStrategy::CustomScheme { .. } => "custom_scheme",
        RedirectStrategy::UniversalLink { .. } => "universal_link",
        RedirectStrategy::ManualPaste => "manual_paste",
    }
}

fn display_url_host(host: IpAddr) -> String {
    match host {
        IpAddr::V4(host) => host.to_string(),
        IpAddr::V6(host) => format!("[{host}]"),
    }
}

fn normalized_path(path: &str) -> String {
    if path.starts_with('/') {
        path.to_owned()
    } else {
        format!("/{path}")
    }
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
