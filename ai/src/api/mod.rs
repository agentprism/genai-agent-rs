//! The wire-protocol seam ⇐ pi `ProviderStreams` (`src/types.ts:260-277`, seam #1).
//!
//! One module per wire protocol, mirroring pi `src/api/` file for file (snake_case is the
//! language-forced rename of pi's kebab-case filenames). A `Provider` holds implementations
//! of this trait as *values* — singly, or in a map keyed by `Model.api` for mixed-API
//! providers (seam #2). Providers never know which SDK or transport backs a model.
//!
//! Contract carried over from pi verbatim:
//! - `stream`/`stream_simple` NEVER fail out-of-band. Every failure — setup, auth,
//!   dispatch ("no API implementation for X"), transport, provider error — is a terminal
//!   `error` event inside the returned stream (pi `lazyStream`, `api/lazy.ts:4-23`;
//!   seams #1/#12). These methods therefore do not return `Result`.
//! - Deferred (background) responses are a capability, not a base method: pi exposes
//!   `fetchDeferred`/`cancelDeferred` only on capable modules. No module in the ported
//!   subset enables it at the current pin (transport verification, 2026-08-19), so the
//!   contract shape exists but no implementation advertises it.
//! - Per-API typed options ride the untyped dispatch path exactly as in pi: the dispatch
//!   shape takes `ApiStreamOptions`; each module also exports typed entry points taking its
//!   concrete options type (pi types.ts:260 "This is the untyped dispatch shape; per-API
//!   option typing lives on the implementation modules themselves"). A variant mismatched
//!   to the implementation is a terminal `error` event, mirroring the checked assertion on
//!   pi's dynamic-dispatch path.

pub mod anthropic_messages;
pub mod bedrock_converse_stream;
pub mod constrained_sampling;
pub mod github_copilot_headers;
pub mod google_generative_ai;
pub mod google_shared;
pub mod google_vertex;
pub mod openai_codex_responses;
pub mod openai_completions;
pub mod openai_prompt_cache;
pub mod openai_responses;
pub mod openai_responses_shared;
mod openai_sse;
pub mod simple_options;
pub mod transform_messages;

use crate::event_stream::AssistantMessageEventStream;
use crate::types::{
    AssistantImages, Context, DeferredCancelOptions, DeferredFetchOptions, DeferredHandle,
    ImagesContext, ImagesError, ImagesModel, ImagesOptions, Model, SimpleStreamOptions,
    StreamOptions,
};
use futures::future::BoxFuture;

/// Options crossing the untyped dispatch path (pi `ApiStreamOptions<TApi>`).
///
/// `Base` covers callers that pass only the shared tier; `Custom` covers custom API strings
/// (pi: `StreamOptions & Record<string, unknown>`). One typed variant is added per ported
/// API implementation as its options type is ported.
#[derive(Debug, Clone)]
pub enum ApiStreamOptions {
    Base(StreamOptions),
    AnthropicMessages(anthropic_messages::AnthropicOptions),
    BedrockConverseStream(bedrock_converse_stream::BedrockOptions),
    GoogleGenerativeAI(google_generative_ai::GoogleOptions),
    GoogleVertex(google_vertex::GoogleVertexOptions),
    OpenAICompletions(openai_completions::OpenAICompletionsOptions),
    OpenAIResponses(openai_responses::OpenAIResponsesOptions),
    OpenAICodexResponses(openai_codex_responses::OpenAICodexResponsesOptions),
    Custom {
        base: StreamOptions,
        extra: serde_json::Map<String, serde_json::Value>,
    },
}

/// pi `ProviderStreams` — the uniform stream contract of an API implementation module.
pub trait ProviderStreams: Send + Sync {
    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: ApiStreamOptions,
    ) -> AssistantMessageEventStream;

    fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: SimpleStreamOptions,
    ) -> AssistantMessageEventStream;

    /// Capability accessor replacing pi's optional-method presence check
    /// (`implementation.fetchDeferred` truthiness in `api/lazy.ts`).
    fn deferred(&self) -> Option<&dyn DeferredStreams> {
        None
    }

    fn supports_fetch_deferred(&self) -> bool {
        self.deferred().is_some()
    }

    fn fetch_deferred(
        &self,
        model: &Model,
        handle: &DeferredHandle,
        options: DeferredFetchOptions,
    ) -> Option<AssistantMessageEventStream> {
        self.deferred()
            .map(|deferred| deferred.fetch_deferred(model, handle, options))
    }

    fn supports_cancel_deferred(&self) -> bool {
        self.deferred().is_some()
    }

    fn cancel_deferred<'a>(
        &'a self,
        model: &'a Model,
        handle: &'a DeferredHandle,
        options: DeferredCancelOptions,
    ) -> Option<BoxFuture<'a, Result<(), crate::types::AssistantMessage>>> {
        self.deferred()
            .map(|deferred| deferred.cancel_deferred(model, handle, options))
    }
}

/// pi's optional deferred-response surface (`fetchDeferred`/`cancelDeferred`).
pub trait DeferredStreams: Send + Sync {
    /// Failures in-band, like `stream` (pi returns an event stream here too).
    fn fetch_deferred(
        &self,
        model: &Model,
        handle: &DeferredHandle,
        options: DeferredFetchOptions,
    ) -> AssistantMessageEventStream;

    /// pi `cancelDeferred` returns `Promise<void>` and MAY reject — the one fallible
    /// method on the seam.
    fn cancel_deferred<'a>(
        &'a self,
        model: &'a Model,
        handle: &'a DeferredHandle,
        options: DeferredCancelOptions,
    ) -> BoxFuture<'a, Result<(), crate::types::AssistantMessage>>;
}

/// pi `ProviderImages` — the uniform contract of an image-generation API module.
pub trait ProviderImages: Send + Sync {
    fn generate_images<'a>(
        &'a self,
        model: &'a ImagesModel,
        context: &'a ImagesContext,
        options: ImagesOptions,
    ) -> BoxFuture<'a, Result<AssistantImages, ImagesError>>;
}
