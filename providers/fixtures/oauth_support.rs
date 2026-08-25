//! Hermetic host and transport doubles for provider OAuth conformance tests.

#![allow(
    dead_code,
    reason = "each provider includes only the portions of this shared test double it exercises"
)]
#![allow(
    clippy::result_large_err,
    reason = "the test transport must implement the public transport error contract exactly"
)]

use http::HeaderMap;
use pi_ai::*;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug)]
pub struct HttpScriptedResponse {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

impl HttpScriptedResponse {
    pub fn json(status: u16, body: &str) -> Self {
        Self {
            status,
            headers: HeaderMap::new(),
            body: body.as_bytes().to_vec(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct SeenRequest {
    pub method: http::Method,
    pub url: String,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

#[derive(Clone, Default)]
pub struct ScriptedTransport {
    responses: Arc<Mutex<VecDeque<HttpScriptedResponse>>>,
    pub seen: Arc<Mutex<Vec<SeenRequest>>>,
}

impl ScriptedTransport {
    pub fn new(responses: impl IntoIterator<Item = HttpScriptedResponse>) -> Self {
        Self {
            responses: Arc::new(Mutex::new(responses.into_iter().collect())),
            seen: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn take(&self, request: HttpRequest) -> Result<HttpScriptedResponse, TransportError> {
        self.seen.lock().unwrap().push(SeenRequest {
            method: request.method,
            url: request.url.to_string(),
            headers: request.headers,
            body: request.body,
        });
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| TransportError::new("unexpected_request", "no scripted response"))
    }
}

impl HttpTransport for ScriptedTransport {
    fn execute(
        &self,
        request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<HttpResponse, TransportError>> {
        let response = self.take(request);
        Box::pin(async move {
            let response = response?;
            Ok(HttpResponse::from_bytes(
                response.status,
                response.headers,
                response.body,
            ))
        })
    }
}

impl LocalHttpTransport for ScriptedTransport {
    fn execute(
        &self,
        request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalHttpResponse, TransportError>> {
        let response = self.take(request);
        Box::pin(async move {
            let response = response?;
            Ok(LocalHttpResponse::from_bytes(
                response.status,
                response.headers,
                response.body,
            ))
        })
    }
}

#[derive(Default)]
pub struct RecordingInteraction {
    pub answers: Mutex<VecDeque<AuthAnswer>>,
    pub notifications: Mutex<Vec<AuthEvent>>,
}

impl RecordingInteraction {
    pub fn with_answers(answers: impl IntoIterator<Item = AuthAnswer>) -> Self {
        Self {
            answers: Mutex::new(answers.into_iter().collect()),
            notifications: Mutex::new(Vec::new()),
        }
    }

    fn answer(&self) -> Result<AuthAnswer, AuthInteractionError> {
        self.answers
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| AuthInteractionError::Failed {
                code: "missing_answer".into(),
                message: "no scripted auth answer".into(),
            })
    }
}

impl AuthInteraction for RecordingInteraction {
    fn capabilities(&self) -> AuthHostCapabilities {
        AuthHostCapabilities::default()
    }

    fn prompt(
        &self,
        _prompt: AuthPrompt,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<AuthAnswer, AuthInteractionError>> {
        let answer = self.answer();
        Box::pin(async move { answer })
    }

    fn notify(&self, event: AuthEvent) -> Result<(), AuthInteractionError> {
        self.notifications.lock().unwrap().push(event);
        Ok(())
    }

    fn create_redirect_receiver(
        &self,
        _request: RedirectReceiverRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Box<dyn RedirectReceiver>, AuthInteractionError>> {
        Box::pin(async {
            Err(AuthInteractionError::Unsupported {
                message: "scripted interaction has no redirect receiver".into(),
            })
        })
    }
}

impl LocalAuthInteraction for RecordingInteraction {
    fn capabilities(&self) -> AuthHostCapabilities {
        AuthHostCapabilities::default()
    }

    fn prompt(
        &self,
        _prompt: AuthPrompt,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<AuthAnswer, AuthInteractionError>> {
        let answer = self.answer();
        Box::pin(async move { answer })
    }

    fn notify(&self, event: AuthEvent) -> Result<(), AuthInteractionError> {
        self.notifications.lock().unwrap().push(event);
        Ok(())
    }

    fn create_redirect_receiver(
        &self,
        _request: RedirectReceiverRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Box<dyn LocalRedirectReceiver>, AuthInteractionError>> {
        Box::pin(async {
            Err(AuthInteractionError::Unsupported {
                message: "scripted interaction has no redirect receiver".into(),
            })
        })
    }
}
