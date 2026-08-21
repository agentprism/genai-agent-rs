use super::*;
use crate::auth::types::{AuthInteraction, AuthPrompt};
use crate::auth::{AuthContext, AuthFuture};
use crate::utils::abort::AbortController;
use indexmap::IndexMap;
use std::collections::VecDeque;
use std::sync::Mutex;

#[derive(Default)]
struct StaticContext {
    env: IndexMap<String, String>,
    files: Vec<String>,
}

struct StoredCredentialContext {
    env_calls: Arc<Mutex<Vec<String>>>,
    credential_path: String,
}

impl AuthContext for StoredCredentialContext {
    fn env(&self, name: String) -> AuthFuture<Option<String>> {
        self.env_calls
            .lock()
            .expect("environment calls lock")
            .push(name.clone());
        Box::pin(async move {
            if name == "GOOGLE_CLOUD_API_KEY" {
                Ok(None)
            } else {
                Err(AuthError::new(format!(
                    "unexpected environment read: {name}"
                )))
            }
        })
    }

    fn file_exists(&self, path: String) -> AuthFuture<bool> {
        let exists = path == self.credential_path;
        Box::pin(async move { Ok(exists) })
    }
}

impl AuthContext for StaticContext {
    fn env(&self, name: String) -> AuthFuture<Option<String>> {
        let value = self.env.get(&name).cloned();
        Box::pin(async move { Ok(value) })
    }

    fn file_exists(&self, path: String) -> AuthFuture<bool> {
        let exists = self.files.contains(&path);
        Box::pin(async move { Ok(exists) })
    }
}

fn input(
    context: StaticContext,
    credential: Option<ApiKeyCredential>,
) -> crate::auth::types::ApiKeyResolveInput {
    crate::auth::types::ApiKeyResolveInput {
        ctx: Arc::new(context),
        credential,
        signal: AbortController::new().signal(),
    }
}

struct QueueInteraction {
    answers: Mutex<VecDeque<String>>,
    events: Mutex<Vec<AuthEvent>>,
}

impl QueueInteraction {
    fn new(answers: impl IntoIterator<Item = &'static str>) -> Self {
        Self {
            answers: Mutex::new(answers.into_iter().map(str::to_owned).collect()),
            events: Mutex::new(Vec::new()),
        }
    }
}

impl AuthInteraction for QueueInteraction {
    fn signal(&self) -> Option<Arc<dyn crate::types::AbortSignal>> {
        None
    }

    fn prompt(&self, _prompt: AuthPrompt) -> AuthFuture<String> {
        let answer = self
            .answers
            .lock()
            .expect("answers lock")
            .pop_front()
            .expect("queued answer");
        Box::pin(async move { Ok(answer) })
    }

    fn notify(&self, event: AuthEvent) {
        self.events.lock().expect("events lock").push(event);
    }
}

/// Ports pi `test/providers.test.ts:214-256`.
#[tokio::test]
async fn vertex_provider_owns_api_key_adc_and_service_account_login_flows() {
    let login = vertex_auth().login.expect("Vertex login");
    let key_interaction = Arc::new(QueueInteraction::new(["api-key", "vertex-key"]));
    let credential = login(ProviderAuthInteraction {
        interaction: key_interaction,
        signal: AbortController::new().signal(),
    })
    .await
    .expect("API-key login");
    assert_eq!(credential.key.as_deref(), Some("vertex-key"));
    assert_eq!(credential.env, None);

    let adc_interaction = Arc::new(QueueInteraction::new(["adc", "project-id", "us-central1"]));
    let credential = login(ProviderAuthInteraction {
        interaction: adc_interaction.clone(),
        signal: AbortController::new().signal(),
    })
    .await
    .expect("ADC login");
    assert_eq!(
        credential.env,
        Some(ProviderEnv::from([
            ("GOOGLE_CLOUD_PROJECT".to_owned(), "project-id".to_owned()),
            ("GOOGLE_CLOUD_LOCATION".to_owned(), "us-central1".to_owned(),),
        ]))
    );
    assert!(matches!(
        adc_interaction.events.lock().expect("events lock").as_slice(),
        [AuthEvent::Info { links: Some(links), .. }]
            if links[0].label.as_deref() == Some("Application Default Credentials")
    ));

    let service_interaction = Arc::new(QueueInteraction::new([
        "service-account",
        "service-project",
        "global",
        "/credentials/service.json",
    ]));
    let credential = login(ProviderAuthInteraction {
        interaction: service_interaction,
        signal: AbortController::new().signal(),
    })
    .await
    .expect("service-account login");
    assert_eq!(
        credential
            .env
            .as_ref()
            .and_then(|env| env.get("GOOGLE_APPLICATION_CREDENTIALS"))
            .map(String::as_str),
        Some("/credentials/service.json")
    );
}

/// Ports the Vertex ADC cases in pi `test/providers.test.ts:260-268` and
/// `src/providers/google-vertex.ts:64-88`.
#[tokio::test]
async fn vertex_auth_resolves_keys_and_adc_with_pi_precedence() {
    let auth = vertex_auth();
    let resolved = (auth.resolve)(input(
        StaticContext {
            env: IndexMap::from([(
                "GOOGLE_CLOUD_API_KEY".to_owned(),
                "environment-key".to_owned(),
            )]),
            ..StaticContext::default()
        },
        Some(ApiKeyCredential {
            kind: ApiKeyCredentialType::ApiKey,
            key: Some("stored-key".to_owned()),
            env: None,
        }),
    ))
    .await
    .expect("resolve")
    .expect("credential");
    assert_eq!(resolved.auth.api_key.as_deref(), Some("stored-key"));
    assert_eq!(resolved.source.as_deref(), Some("stored credential"));

    let stored_env = ProviderEnv::from([
        (
            "GOOGLE_CLOUD_PROJECT".to_owned(),
            "stored-project".to_owned(),
        ),
        ("GOOGLE_CLOUD_LOCATION".to_owned(), "global".to_owned()),
        (
            "GOOGLE_APPLICATION_CREDENTIALS".to_owned(),
            "/credentials/service-account.json".to_owned(),
        ),
    ]);
    let resolved = (auth.resolve)(input(
        StaticContext {
            files: vec!["/credentials/service-account.json".to_owned()],
            ..StaticContext::default()
        },
        Some(ApiKeyCredential {
            kind: ApiKeyCredentialType::ApiKey,
            key: None,
            env: Some(stored_env.clone()),
        }),
    ))
    .await
    .expect("resolve")
    .expect("credential");
    assert_eq!(resolved.auth, ModelAuth::default());
    assert_eq!(resolved.env, Some(stored_env));
    assert_eq!(resolved.source.as_deref(), Some("stored credential"));

    let resolved = (auth.resolve)(input(
        StaticContext {
            env: IndexMap::from([
                ("GOOGLE_CLOUD_PROJECT".to_owned(), "project".to_owned()),
                ("GOOGLE_CLOUD_LOCATION".to_owned(), "us-central1".to_owned()),
            ]),
            files: vec![VERTEX_ADC_PATH.to_owned()],
        },
        None,
    ))
    .await
    .expect("resolve")
    .expect("credential");
    assert_eq!(resolved.auth, ModelAuth::default());
    assert_eq!(
        resolved.source.as_deref(),
        Some("gcloud application default credentials")
    );

    let unresolved = (auth.resolve)(input(
        StaticContext {
            files: vec![VERTEX_ADC_PATH.to_owned()],
            ..StaticContext::default()
        },
        Some(ApiKeyCredential {
            kind: ApiKeyCredentialType::ApiKey,
            key: None,
            env: Some(ProviderEnv::from([
                ("GOOGLE_CLOUD_PROJECT".to_owned(), String::new()),
                ("GOOGLE_CLOUD_LOCATION".to_owned(), "global".to_owned()),
            ])),
        }),
    ))
    .await
    .expect("resolve");
    assert_eq!(unresolved, None);
}

/// Pins the nullish-coalescing short circuit in pi `src/providers/google-vertex.ts:74-80`.
#[tokio::test]
async fn stored_vertex_environment_does_not_read_fallback_environment_names() {
    let env_calls = Arc::new(Mutex::new(Vec::new()));
    let credential_path = "/credentials/stored-service-account.json";
    let context = Arc::new(StoredCredentialContext {
        env_calls: env_calls.clone(),
        credential_path: credential_path.to_owned(),
    });
    let credential = ApiKeyCredential {
        kind: ApiKeyCredentialType::ApiKey,
        key: None,
        env: Some(ProviderEnv::from([
            (
                "GOOGLE_APPLICATION_CREDENTIALS".to_owned(),
                credential_path.to_owned(),
            ),
            (
                "GOOGLE_CLOUD_PROJECT".to_owned(),
                "stored-project".to_owned(),
            ),
            (
                "GOOGLE_CLOUD_LOCATION".to_owned(),
                "stored-location".to_owned(),
            ),
        ])),
    };

    let resolved = (vertex_auth().resolve)(crate::auth::types::ApiKeyResolveInput {
        ctx: context,
        credential: Some(credential),
        signal: AbortController::new().signal(),
    })
    .await
    .expect("stored credential resolution")
    .expect("stored credential");

    assert_eq!(resolved.source.as_deref(), Some("stored credential"));
    assert_eq!(
        env_calls.lock().expect("environment calls lock").as_slice(),
        ["GOOGLE_CLOUD_API_KEY"]
    );
}
