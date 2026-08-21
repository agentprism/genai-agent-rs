use ai::api::ProviderStreams;
use ai::compat::{
    ApiKeyAuth, ApiKeyCredential, AuthContext, Credential, CredentialStore,
    DefaultProviderAuthContext, InMemoryCredentialStore, OAuthAuth, OAuthCredential, ProviderAuth,
    anthropic_messages_api, bedrock_converse_stream_api, default_provider_auth_context,
    env_api_key_auth, google_generative_ai_api, google_vertex_api, lazy_oauth,
    open_ai_codex_responses_api, open_ai_completions_api, open_ai_responses_api,
};
use std::marker::PhantomData;

fn assert_auth_context<T: AuthContext>() {}
fn assert_credential_store<T: CredentialStore>() {}
fn assert_provider_streams<T: ProviderStreams>(_streams: T) {}

/// Pins pi `src/compat.ts:13-21,27` and `src/index.ts:21-24`.
#[test]
fn compat_flattens_auth_and_eager_api_factory_exports() {
    assert_auth_context::<DefaultProviderAuthContext>();
    assert_credential_store::<InMemoryCredentialStore>();
    let _ = default_provider_auth_context();
    let _: PhantomData<ApiKeyAuth> = PhantomData;
    let _: PhantomData<OAuthAuth> = PhantomData;
    let _: PhantomData<ProviderAuth> = PhantomData;
    let _: PhantomData<ApiKeyCredential> = PhantomData;
    let _: PhantomData<OAuthCredential> = PhantomData;
    let _: PhantomData<Credential> = PhantomData;
    let _ = env_api_key_auth("API key", Vec::new());
    let _ = lazy_oauth;

    assert_provider_streams(anthropic_messages_api());
    assert_provider_streams(bedrock_converse_stream_api());
    assert_provider_streams(google_generative_ai_api());
    assert_provider_streams(google_vertex_api());
    assert_provider_streams(open_ai_codex_responses_api());
    assert_provider_streams(open_ai_completions_api());
    assert_provider_streams(open_ai_responses_api());
}
