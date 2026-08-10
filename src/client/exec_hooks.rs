//! Per-request exec hooks that library users can set on the Client to observe or intercept
//! the chat execution web calls (`exec_chat` and `exec_chat_stream`).
//!
//! - [`PayloadInterceptor`] receives the target [`ModelIden`] and the serialized provider payload
//!   (`serde_json::Value`) right before the HTTP request is built, and can replace the payload.
//!
//! - [`ResponseObserver`] receives the target [`ModelIden`], the response `StatusCode`, and the
//!   response `HeaderMap` as soon as the HTTP response arrives, before the body/stream is consumed
//!   (including on 4xx/5xx responses).
//!
//! Both follow the resolver idiom (see `AuthResolver`): dedicated types with sync and async
//! function variants, installed via the `ClientBuilder` and stored in the `ClientConfig`.
//!
//! Construction-time hooks are the client-wide default for their channel. A single request can
//! override each channel independently through [`ExecOptions`] (`exec_chat_with_exec_options` /
//! `exec_chat_stream_with_exec_options`): each channel resolves to exactly one state —
//! [`ExecHookOverride::Inherit`] (use the client default), [`ExecHookOverride::Replace`] (use the
//! request's hook instead), or [`ExecHookOverride::Disable`] (no hook for this request) — so at
//! most one hook ever fires per channel per HTTP attempt, and a request replacement never
//! composes with the construction-time default.

use crate::ModelIden;
use reqwest::StatusCode;
use reqwest::header::HeaderMap;
use serde_json::Value;
use std::pin::Pin;
use std::sync::Arc;

// region:    --- PayloadInterceptor

/// Holder for the payload interceptor function.
///
/// The interceptor is called once per chat exec call (streaming and non-streaming), after the
/// adapter serialized the provider payload and before the HTTP request is built. Returning
/// `Some(value)` replaces the payload sent over the wire; returning `None` keeps it unchanged.
///
/// Note: When an interceptor is set, the payload is cloned once per request to hand it to the
/// interceptor by value (no clone occurs when no interceptor is configured).
#[derive(Debug, Clone)]
pub enum PayloadInterceptor {
	/// The `PayloadInterceptorFn` trait object (sync).
	InterceptorFn(Arc<Box<dyn PayloadInterceptorFn>>),
	/// The `PayloadInterceptorAsyncFn` trait object (async).
	InterceptorAsyncFn(Arc<Box<dyn PayloadInterceptorAsyncFn>>),
}

impl PayloadInterceptor {
	/// Create a new `PayloadInterceptor` from a sync interceptor function.
	pub fn from_interceptor_fn(interceptor_fn: impl IntoPayloadInterceptorFn) -> Self {
		PayloadInterceptor::InterceptorFn(interceptor_fn.into_interceptor_fn())
	}

	/// Create a new `PayloadInterceptor` from an async interceptor function.
	pub fn from_interceptor_async_fn(interceptor_fn: impl IntoPayloadInterceptorAsyncFn) -> Self {
		PayloadInterceptor::InterceptorAsyncFn(interceptor_fn.into_async_interceptor_fn())
	}
}

impl PayloadInterceptor {
	pub(crate) async fn intercept(&self, model_iden: ModelIden, payload: Value) -> Option<Value> {
		match self {
			PayloadInterceptor::InterceptorFn(interceptor_fn) => interceptor_fn.clone().exec_fn(model_iden, payload),
			PayloadInterceptor::InterceptorAsyncFn(interceptor_fn) => interceptor_fn.exec_fn(model_iden, payload).await,
		}
	}
}

// endregion: --- PayloadInterceptor

// region:    --- PayloadInterceptorFn

/// The `PayloadInterceptorFn` trait object (sync variant).
pub trait PayloadInterceptorFn: Send + Sync {
	/// Execute the interceptor. `Some` replaces the payload; `None` keeps it unchanged.
	fn exec_fn(&self, model_iden: ModelIden, payload: Value) -> Option<Value>;

	/// Clone the trait object.
	fn clone_box(&self) -> Box<dyn PayloadInterceptorFn>;
}

/// `PayloadInterceptorFn` blanket implementation for any function matching the signature.
impl<F> PayloadInterceptorFn for F
where
	F: FnOnce(ModelIden, Value) -> Option<Value> + Send + Sync + Clone + 'static,
{
	fn exec_fn(&self, model_iden: ModelIden, payload: Value) -> Option<Value> {
		(self.clone())(model_iden, payload)
	}

	fn clone_box(&self) -> Box<dyn PayloadInterceptorFn> {
		Box::new(self.clone())
	}
}

impl Clone for Box<dyn PayloadInterceptorFn> {
	fn clone(&self) -> Self {
		self.clone_box()
	}
}

impl std::fmt::Debug for dyn PayloadInterceptorFn {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "PayloadInterceptorFn")
	}
}

/// Custom and convenient trait used in the `PayloadInterceptor::from_interceptor_fn` argument.
pub trait IntoPayloadInterceptorFn {
	/// Convert the argument into a `PayloadInterceptorFn` trait object.
	fn into_interceptor_fn(self) -> Arc<Box<dyn PayloadInterceptorFn>>;
}

impl IntoPayloadInterceptorFn for Arc<Box<dyn PayloadInterceptorFn>> {
	fn into_interceptor_fn(self) -> Arc<Box<dyn PayloadInterceptorFn>> {
		self
	}
}

// Implement `IntoPayloadInterceptorFn` for closures.
impl<F> IntoPayloadInterceptorFn for F
where
	F: FnOnce(ModelIden, Value) -> Option<Value> + Send + Sync + Clone + 'static,
{
	fn into_interceptor_fn(self) -> Arc<Box<dyn PayloadInterceptorFn>> {
		Arc::new(Box::new(self))
	}
}

// endregion: --- PayloadInterceptorFn

// region:    --- PayloadInterceptorAsyncFn

/// The `PayloadInterceptorAsyncFn` trait object (async variant).
pub trait PayloadInterceptorAsyncFn: Send + Sync {
	/// Execute the interceptor. `Some` replaces the payload; `None` keeps it unchanged.
	fn exec_fn(&self, model_iden: ModelIden, payload: Value) -> Pin<Box<dyn Future<Output = Option<Value>> + Send>>;

	/// Clone the trait object.
	fn clone_box(&self) -> Box<dyn PayloadInterceptorAsyncFn>;
}

impl<F> PayloadInterceptorAsyncFn for F
where
	F: Fn(ModelIden, Value) -> Pin<Box<dyn Future<Output = Option<Value>> + Send>> + Send + Sync + Clone + 'static,
{
	fn exec_fn(&self, model_iden: ModelIden, payload: Value) -> Pin<Box<dyn Future<Output = Option<Value>> + Send>> {
		self(model_iden, payload)
	}

	fn clone_box(&self) -> Box<dyn PayloadInterceptorAsyncFn> {
		Box::new(self.clone())
	}
}

impl Clone for Box<dyn PayloadInterceptorAsyncFn> {
	fn clone(&self) -> Self {
		self.clone_box()
	}
}

impl std::fmt::Debug for dyn PayloadInterceptorAsyncFn {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "PayloadInterceptorAsyncFn")
	}
}

/// Custom and convenient trait used in the `PayloadInterceptor::from_interceptor_async_fn` argument.
pub trait IntoPayloadInterceptorAsyncFn {
	/// Convert the argument into a `PayloadInterceptorAsyncFn` trait object.
	fn into_async_interceptor_fn(self) -> Arc<Box<dyn PayloadInterceptorAsyncFn>>;
}

impl IntoPayloadInterceptorAsyncFn for Arc<Box<dyn PayloadInterceptorAsyncFn>> {
	fn into_async_interceptor_fn(self) -> Arc<Box<dyn PayloadInterceptorAsyncFn>> {
		self
	}
}

impl<F> IntoPayloadInterceptorAsyncFn for F
where
	F: Fn(ModelIden, Value) -> Pin<Box<dyn Future<Output = Option<Value>> + Send>> + Send + Sync + Clone + 'static,
{
	fn into_async_interceptor_fn(self) -> Arc<Box<dyn PayloadInterceptorAsyncFn>> {
		Arc::new(Box::new(self))
	}
}

// endregion: --- PayloadInterceptorAsyncFn

// region:    --- ResponseObserver

/// Holder for the response observer function.
///
/// The observer is called once per chat exec call (streaming and non-streaming) with the target
/// [`ModelIden`], the response `StatusCode`, and the response `HeaderMap`, as soon as the HTTP
/// response arrives and before its body/stream is consumed. It also fires on 4xx/5xx responses.
///
/// Note: On the streaming path, the HTTP request is sent lazily on the first stream poll, so the
/// observer fires during stream consumption (not at `exec_chat_stream` return time).
#[derive(Debug, Clone)]
pub enum ResponseObserver {
	/// The `ResponseObserverFn` trait object (sync).
	ObserverFn(Arc<Box<dyn ResponseObserverFn>>),
	/// The `ResponseObserverAsyncFn` trait object (async).
	ObserverAsyncFn(Arc<Box<dyn ResponseObserverAsyncFn>>),
}

impl ResponseObserver {
	/// Create a new `ResponseObserver` from a sync observer function.
	pub fn from_observer_fn(observer_fn: impl IntoResponseObserverFn) -> Self {
		ResponseObserver::ObserverFn(observer_fn.into_observer_fn())
	}

	/// Create a new `ResponseObserver` from an async observer function.
	pub fn from_observer_async_fn(observer_fn: impl IntoResponseObserverAsyncFn) -> Self {
		ResponseObserver::ObserverAsyncFn(observer_fn.into_async_observer_fn())
	}
}

impl ResponseObserver {
	pub(crate) async fn observe(&self, model_iden: ModelIden, status: StatusCode, headers: HeaderMap) {
		match self {
			ResponseObserver::ObserverFn(observer_fn) => observer_fn.clone().exec_fn(model_iden, status, headers),
			ResponseObserver::ObserverAsyncFn(observer_fn) => observer_fn.exec_fn(model_iden, status, headers).await,
		}
	}
}

// endregion: --- ResponseObserver

// region:    --- ResponseObserverFn

/// The `ResponseObserverFn` trait object (sync variant).
pub trait ResponseObserverFn: Send + Sync {
	/// Execute the observer with the response status and headers.
	fn exec_fn(&self, model_iden: ModelIden, status: StatusCode, headers: HeaderMap);

	/// Clone the trait object.
	fn clone_box(&self) -> Box<dyn ResponseObserverFn>;
}

/// `ResponseObserverFn` blanket implementation for any function matching the signature.
impl<F> ResponseObserverFn for F
where
	F: FnOnce(ModelIden, StatusCode, HeaderMap) + Send + Sync + Clone + 'static,
{
	fn exec_fn(&self, model_iden: ModelIden, status: StatusCode, headers: HeaderMap) {
		(self.clone())(model_iden, status, headers)
	}

	fn clone_box(&self) -> Box<dyn ResponseObserverFn> {
		Box::new(self.clone())
	}
}

impl Clone for Box<dyn ResponseObserverFn> {
	fn clone(&self) -> Self {
		self.clone_box()
	}
}

impl std::fmt::Debug for dyn ResponseObserverFn {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "ResponseObserverFn")
	}
}

/// Custom and convenient trait used in the `ResponseObserver::from_observer_fn` argument.
pub trait IntoResponseObserverFn {
	/// Convert the argument into a `ResponseObserverFn` trait object.
	fn into_observer_fn(self) -> Arc<Box<dyn ResponseObserverFn>>;
}

impl IntoResponseObserverFn for Arc<Box<dyn ResponseObserverFn>> {
	fn into_observer_fn(self) -> Arc<Box<dyn ResponseObserverFn>> {
		self
	}
}

// Implement `IntoResponseObserverFn` for closures.
impl<F> IntoResponseObserverFn for F
where
	F: FnOnce(ModelIden, StatusCode, HeaderMap) + Send + Sync + Clone + 'static,
{
	fn into_observer_fn(self) -> Arc<Box<dyn ResponseObserverFn>> {
		Arc::new(Box::new(self))
	}
}

// endregion: --- ResponseObserverFn

// region:    --- ResponseObserverAsyncFn

/// The `ResponseObserverAsyncFn` trait object (async variant).
pub trait ResponseObserverAsyncFn: Send + Sync {
	/// Execute the observer with the response status and headers.
	fn exec_fn(
		&self,
		model_iden: ModelIden,
		status: StatusCode,
		headers: HeaderMap,
	) -> Pin<Box<dyn Future<Output = ()> + Send>>;

	/// Clone the trait object.
	fn clone_box(&self) -> Box<dyn ResponseObserverAsyncFn>;
}

impl<F> ResponseObserverAsyncFn for F
where
	F: Fn(ModelIden, StatusCode, HeaderMap) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync + Clone + 'static,
{
	fn exec_fn(
		&self,
		model_iden: ModelIden,
		status: StatusCode,
		headers: HeaderMap,
	) -> Pin<Box<dyn Future<Output = ()> + Send>> {
		self(model_iden, status, headers)
	}

	fn clone_box(&self) -> Box<dyn ResponseObserverAsyncFn> {
		Box::new(self.clone())
	}
}

impl Clone for Box<dyn ResponseObserverAsyncFn> {
	fn clone(&self) -> Self {
		self.clone_box()
	}
}

impl std::fmt::Debug for dyn ResponseObserverAsyncFn {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "ResponseObserverAsyncFn")
	}
}

/// Custom and convenient trait used in the `ResponseObserver::from_observer_async_fn` argument.
pub trait IntoResponseObserverAsyncFn {
	/// Convert the argument into a `ResponseObserverAsyncFn` trait object.
	fn into_async_observer_fn(self) -> Arc<Box<dyn ResponseObserverAsyncFn>>;
}

impl IntoResponseObserverAsyncFn for Arc<Box<dyn ResponseObserverAsyncFn>> {
	fn into_async_observer_fn(self) -> Arc<Box<dyn ResponseObserverAsyncFn>> {
		self
	}
}

impl<F> IntoResponseObserverAsyncFn for F
where
	F: Fn(ModelIden, StatusCode, HeaderMap) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync + Clone + 'static,
{
	fn into_async_observer_fn(self) -> Arc<Box<dyn ResponseObserverAsyncFn>> {
		Arc::new(Box::new(self))
	}
}

// endregion: --- ResponseObserverAsyncFn

// region:    --- BoundResponseObserver

/// Crate plumbing: a [`ResponseObserver`] bound to the [`ModelIden`] of the in-flight request.
///
/// Carried into the web layer (`WebStream` and friends) so the observer can fire when the
/// `reqwest::Response` first materializes — before any status check or body consumption —
/// without the web layer having to know about model resolution.
#[derive(Debug, Clone)]
pub(crate) struct BoundResponseObserver {
	model_iden: ModelIden,
	observer: ResponseObserver,
}

impl BoundResponseObserver {
	pub(crate) fn new(observer: ResponseObserver, model_iden: ModelIden) -> Self {
		Self { model_iden, observer }
	}

	pub(crate) async fn observe(&self, status: StatusCode, headers: HeaderMap) {
		self.observer.observe(self.model_iden.clone(), status, headers).await;
	}
}

// endregion: --- BoundResponseObserver

// region:    --- ExecHookOverride

/// Per-request override state for one exec-hook channel.
///
/// Each channel ([`PayloadInterceptor`] / [`ResponseObserver`]) resolves independently, and the
/// resolved state is applied exactly once per HTTP attempt: a retry loop that re-issues the
/// request resolves and fires the hooks again for the new attempt.
#[derive(Debug, Clone, Default)]
pub enum ExecHookOverride<T> {
	/// Use the `Client`'s construction-time hook for this channel, if any (the default).
	#[default]
	Inherit,
	/// Use this hook for the request *instead of* the construction-time hook.
	///
	/// The construction-time hook is not called for the request: the two never compose, so the
	/// request hook observes the unmodified serialized payload (payload channel) and exactly one
	/// hook invocation happens per attempt.
	Replace(T),
	/// Fire no hook for this channel on this request, even when a construction-time hook exists.
	Disable,
}

impl<T> ExecHookOverride<T> {
	/// Resolve this override against the construction-time default hook.
	///
	/// Exactly one outcome per channel: the default hook (`Inherit`), the request hook
	/// (`Replace`), or no hook (`Disable`, or `Inherit` with no default configured).
	pub(crate) fn resolve<'a>(&'a self, default: Option<&'a T>) -> Option<&'a T> {
		match self {
			ExecHookOverride::Inherit => default,
			ExecHookOverride::Replace(hook) => Some(hook),
			ExecHookOverride::Disable => None,
		}
	}
}

// endregion: --- ExecHookOverride

// region:    --- ExecOptions

/// Request-scoped exec options for the chat exec calls.
///
/// Accepted by [`Client::exec_chat_with_exec_options`](crate::Client::exec_chat_with_exec_options)
/// and [`Client::exec_chat_stream_with_exec_options`](crate::Client::exec_chat_stream_with_exec_options);
/// the classic [`Client::exec_chat`](crate::Client::exec_chat) /
/// [`Client::exec_chat_stream`](crate::Client::exec_chat_stream) delegate with an
/// all-[`ExecHookOverride::Inherit`] value, which is byte-identical to the pre-override behavior.
///
/// Each channel is resolved independently and fires at most one hook per HTTP attempt — including
/// failed (4xx/5xx) attempts observed by the response channel. A request replacement never
/// composes with the construction-time default.
///
/// The struct is `#[non_exhaustive]`: construct it with [`ExecOptions::new`] (all channels
/// inherit) and select each channel's state with the `with_*` / `without_*` builders.
///
/// [`Client`]: crate::Client
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct ExecOptions {
	/// Payload channel override state (see [`ExecHookOverride`]).
	pub payload_interceptor: ExecHookOverride<PayloadInterceptor>,
	/// Response channel override state (see [`ExecHookOverride`]).
	pub response_observer: ExecHookOverride<ResponseObserver>,
}

impl ExecOptions {
	/// Request exec options with every channel in [`ExecHookOverride::Inherit`] state.
	///
	/// This is exactly what [`Client::exec_chat`](crate::Client::exec_chat) /
	/// [`Client::exec_chat_stream`](crate::Client::exec_chat_stream) use.
	pub fn new() -> Self {
		Self::default()
	}

	/// Replace the payload interceptor for this request (construction-time default is not called).
	pub fn with_payload_interceptor(mut self, payload_interceptor: PayloadInterceptor) -> Self {
		self.payload_interceptor = ExecHookOverride::Replace(payload_interceptor);
		self
	}

	/// Replace the payload interceptor for this request from a sync interceptor function.
	pub fn with_payload_interceptor_fn(mut self, payload_interceptor_fn: impl IntoPayloadInterceptorFn) -> Self {
		self.payload_interceptor =
			ExecHookOverride::Replace(PayloadInterceptor::from_interceptor_fn(payload_interceptor_fn));
		self
	}

	/// Set the payload channel override state explicitly.
	pub fn with_payload_interceptor_override(mut self, override_: ExecHookOverride<PayloadInterceptor>) -> Self {
		self.payload_interceptor = override_;
		self
	}

	/// Disable the payload interceptor for this request, even when a construction-time one exists.
	pub fn without_payload_interceptor(mut self) -> Self {
		self.payload_interceptor = ExecHookOverride::Disable;
		self
	}

	/// Replace the response observer for this request (construction-time default is not called).
	pub fn with_response_observer(mut self, response_observer: ResponseObserver) -> Self {
		self.response_observer = ExecHookOverride::Replace(response_observer);
		self
	}

	/// Replace the response observer for this request from a sync observer function.
	pub fn with_response_observer_fn(mut self, response_observer_fn: impl IntoResponseObserverFn) -> Self {
		self.response_observer = ExecHookOverride::Replace(ResponseObserver::from_observer_fn(response_observer_fn));
		self
	}

	/// Set the response channel override state explicitly.
	pub fn with_response_observer_override(mut self, override_: ExecHookOverride<ResponseObserver>) -> Self {
		self.response_observer = override_;
		self
	}

	/// Disable the response observer for this request, even when a construction-time one exists.
	pub fn without_response_observer(mut self) -> Self {
		self.response_observer = ExecHookOverride::Disable;
		self
	}
}

// endregion: --- ExecOptions
