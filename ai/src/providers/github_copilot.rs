use super::github_copilot_models::GITHUB_COPILOT_MODELS;
use crate::api::ProviderStreams;
use crate::api::anthropic_messages::anthropic_messages_api;
use crate::api::openai_completions::open_ai_completions_api;
use crate::api::openai_responses::open_ai_responses_api;
use crate::auth::oauth::load::load_github_copilot_oauth;
use crate::auth::{Credential, ProviderAuth, env_api_key_auth, lazy_oauth};
use crate::models::{CreateProviderOptions, ProviderApi, ProviderRef, create_provider};
use crate::types::Api;
use indexmap::IndexMap;
use std::collections::BTreeSet;
use std::sync::Arc;

pub fn github_copilot_provider() -> ProviderRef {
    let anthropic: Arc<dyn ProviderStreams> = Arc::new(anthropic_messages_api());
    let completions: Arc<dyn ProviderStreams> = Arc::new(open_ai_completions_api());
    let responses: Arc<dyn ProviderStreams> = Arc::new(open_ai_responses_api());
    create_provider(CreateProviderOptions {
        id: "github-copilot".to_owned(),
        name: Some("GitHub Copilot".to_owned()),
        base_url: Some("https://api.individual.githubcopilot.com".to_owned()),
        headers: None,
        auth: ProviderAuth {
            api_key: Some(env_api_key_auth(
                "GitHub Copilot token",
                vec!["COPILOT_GITHUB_TOKEN".to_owned()],
            )),
            oauth: Some(lazy_oauth(
                "GitHub Copilot".to_owned(),
                Some(true),
                None,
                Arc::new(load_github_copilot_oauth),
            )),
        },
        models: GITHUB_COPILOT_MODELS.values().cloned().collect(),
        fetch_models: None,
        filter_models: Some(Arc::new(|models, credential| {
            let Some(Credential::OAuth(credential)) = credential else {
                return Ok(models);
            };
            let Some(ids) = credential
                .extra
                .get("availableModelIds")
                .and_then(|value| value.as_array())
                .filter(|ids| ids.iter().all(|id| id.is_string()))
            else {
                return Ok(models);
            };
            let available = ids
                .iter()
                .filter_map(|id| id.as_str())
                .collect::<BTreeSet<_>>();
            Ok(models
                .into_iter()
                .filter(|model| available.contains(model.id.as_str()))
                .collect())
        })),
        api: ProviderApi::ByApi(IndexMap::from([
            (Api::from("anthropic-messages"), anthropic),
            (Api::from("openai-completions"), completions),
            (Api::from("openai-responses"), responses),
        ])),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{OAuthCredential, OAuthCredentialType};
    use serde_json::{Map, json};

    fn oauth(extra: Map<String, serde_json::Value>) -> Credential {
        Credential::OAuth(OAuthCredential {
            kind: OAuthCredentialType::OAuth,
            refresh: "refresh".to_owned(),
            access: "access".to_owned(),
            expires: f64::MAX,
            extra,
        })
    }

    /// Ports pi `src/providers/github-copilot.ts:19-27` and
    /// `test/github-copilot-anthropic.test.ts:7-126`'s model-availability filter.
    #[test]
    fn oauth_available_model_ids_filter_only_when_all_entries_are_strings() {
        let provider = github_copilot_provider();
        let models = provider.get_models().expect("models");
        let first = models[0].id.clone();
        let filtered = provider
            .filter_models(
                models.clone(),
                Some(&oauth(Map::from_iter([(
                    "availableModelIds".to_owned(),
                    json!([first]),
                )]))),
            )
            .expect("filter");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].id, models[0].id);

        let malformed = provider
            .filter_models(
                models.clone(),
                Some(&oauth(Map::from_iter([(
                    "availableModelIds".to_owned(),
                    json!([models[0].id, 1]),
                )]))),
            )
            .expect("filter");
        assert_eq!(malformed, models);
    }
}
