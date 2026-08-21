use crate::api::{ApiStreamOptions, ProviderStreams};
use crate::event_stream::AssistantMessageEventStream;
use crate::types::{Context, Model, ProviderEnv, SimpleStreamOptions};
use std::sync::Arc;

const CLOUDFLARE_ACCOUNT_ID: &str = "CLOUDFLARE_ACCOUNT_ID";
const CLOUDFLARE_GATEWAY_ID: &str = "CLOUDFLARE_GATEWAY_ID";

pub fn resolve_cloudflare_model(model: &Model, env: Option<&ProviderEnv>) -> Model {
    let Some(env) = env else {
        return model.clone();
    };
    let mut resolved = model.clone();
    resolved.base_url = resolved.base_url.replace(
        "{CLOUDFLARE_ACCOUNT_ID}",
        env.get(CLOUDFLARE_ACCOUNT_ID)
            .map_or("{CLOUDFLARE_ACCOUNT_ID}", String::as_str),
    );
    resolved.base_url = resolved.base_url.replace(
        "{CLOUDFLARE_GATEWAY_ID}",
        env.get(CLOUDFLARE_GATEWAY_ID)
            .map_or("{CLOUDFLARE_GATEWAY_ID}", String::as_str),
    );
    resolved
}

#[derive(Clone)]
struct CloudflareStreams {
    inner: Arc<dyn ProviderStreams>,
}

impl ProviderStreams for CloudflareStreams {
    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: ApiStreamOptions,
    ) -> AssistantMessageEventStream {
        let env = match &options {
            ApiStreamOptions::Base(options) => options.request.env.as_ref(),
            ApiStreamOptions::AnthropicMessages(options) => options.stream.request.env.as_ref(),
            ApiStreamOptions::BedrockConverseStream(options) => options.stream.request.env.as_ref(),
            ApiStreamOptions::OpenAICompletions(options) => options.stream.request.env.as_ref(),
            ApiStreamOptions::OpenAIResponses(options) => options.stream.request.env.as_ref(),
            ApiStreamOptions::OpenAICodexResponses(options) => options.stream.request.env.as_ref(),
            ApiStreamOptions::GoogleGenerativeAI(options) => options.stream.request.env.as_ref(),
            ApiStreamOptions::GoogleVertex(options) => options.stream.request.env.as_ref(),
            ApiStreamOptions::Custom { base, .. } => base.request.env.as_ref(),
        };
        self.inner
            .stream(&resolve_cloudflare_model(model, env), context, options)
    }

    fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        let resolved = resolve_cloudflare_model(model, options.stream.request.env.as_ref());
        self.inner.stream_simple(&resolved, context, options)
    }
}

pub fn cloudflare_streams(streams: Arc<dyn ProviderStreams>) -> Arc<dyn ProviderStreams> {
    Arc::new(CloudflareStreams { inner: streams })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::ApiStreamOptions;
    use crate::event_stream::AssistantMessageEventStream;
    use crate::models_generated::MODELS;
    use std::sync::{Mutex, PoisonError};

    #[derive(Clone, Default)]
    struct RecordingStreams {
        urls: Arc<Mutex<Vec<String>>>,
    }

    impl ProviderStreams for RecordingStreams {
        fn stream(
            &self,
            model: &Model,
            _context: &Context,
            _options: ApiStreamOptions,
        ) -> AssistantMessageEventStream {
            self.urls
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(model.base_url.clone());
            AssistantMessageEventStream::channel().1
        }

        fn stream_simple(
            &self,
            model: &Model,
            _context: &Context,
            _options: SimpleStreamOptions,
        ) -> AssistantMessageEventStream {
            self.urls
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(model.base_url.clone());
            AssistantMessageEventStream::channel().1
        }
    }

    /// Ports pi `src/providers/cloudflare-stream.ts:6-15`.
    #[test]
    fn substitutes_each_available_cloudflare_path_component() {
        let model = &MODELS["cloudflare-workers-ai"]["@cf/openai/gpt-oss-120b"];
        let resolved = resolve_cloudflare_model(
            model,
            Some(&ProviderEnv::from([(
                CLOUDFLARE_ACCOUNT_ID.to_owned(),
                "account".to_owned(),
            )])),
        );
        assert_eq!(
            resolved.base_url,
            "https://api.cloudflare.com/client/v4/accounts/account/ai/v1"
        );
        assert_eq!(
            model.base_url,
            "https://api.cloudflare.com/client/v4/accounts/{CLOUDFLARE_ACCOUNT_ID}/ai/v1"
        );
    }

    /// Ports pi `test/cloudflare-stream.test.ts:22-55`.
    #[test]
    fn materializes_before_both_dispatch_paths_and_keeps_missing_placeholders() {
        let recorder = RecordingStreams::default();
        let urls = recorder.urls.clone();
        let streams = cloudflare_streams(Arc::new(recorder));
        let mut model = MODELS["cloudflare-workers-ai"]["@cf/openai/gpt-oss-120b"].clone();
        model.base_url = "https://gateway.ai.cloudflare.com/v1/{CLOUDFLARE_ACCOUNT_ID}/{CLOUDFLARE_GATEWAY_ID}/openai".to_owned();
        let env = ProviderEnv::from([
            (CLOUDFLARE_ACCOUNT_ID.to_owned(), "account".to_owned()),
            (CLOUDFLARE_GATEWAY_ID.to_owned(), "gateway".to_owned()),
        ]);
        streams.stream(
            &model,
            &Context::default(),
            ApiStreamOptions::Base(crate::types::StreamOptions {
                request: crate::types::ProviderRequestOptions {
                    env: Some(env.clone()),
                    ..Default::default()
                },
                ..Default::default()
            }),
        );
        streams.stream_simple(
            &model,
            &Context::default(),
            SimpleStreamOptions {
                stream: crate::types::StreamOptions {
                    request: crate::types::ProviderRequestOptions {
                        env: Some(env),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        streams.stream_simple(&model, &Context::default(), SimpleStreamOptions::default());
        assert_eq!(
            *urls.lock().unwrap_or_else(PoisonError::into_inner),
            [
                "https://gateway.ai.cloudflare.com/v1/account/gateway/openai",
                "https://gateway.ai.cloudflare.com/v1/account/gateway/openai",
                model.base_url.as_str(),
            ]
        );
    }
}
