//! Binding-friendly product surface for foreign consumers — the analog of
//! liter-llm's `bindings.rs`. This file owns construction from plain scalar
//! and JSON values only; every product method lives on the concrete exported
//! types (`Client` accessors, [`Agent`], [`Run`]) in their own source files.

use std::collections::BTreeMap;
use std::sync::Arc;

use agentprism_ai::{FileCredentialStore, HttpTransport, ModelRef, Models, ReasoningLevel};
use agentprism_bedrock::BedrockSigner;
use agentprism_core::{AgentSnapshot, AgentState, CustomRecordKinds, ToolRegistry};
use agentprism_providers_all::{BuiltinProviderInputs, builtin_providers};

pub use agentprism_runtime_tokio::{TokioAgentHandle as Agent, TokioAgentRun as Run};

/// Shared production HTTP transport plus the Bedrock signing boundary.
type NativeTransports = (Arc<dyn HttpTransport>, Arc<dyn BedrockSigner>);

/// Scalar client configuration; the JSON form accepts the same fields.
#[derive(Clone, Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientConfig {
    /// Durable credential store path; `None` keeps credentials in memory.
    pub auth_store_path: Option<String>,
    /// Construction-time provider environment (API keys, endpoint overrides).
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
}

/// The AgentPrism client: the full model/provider/auth/catalog control plane
/// plus the Rust-owned executor every exported object runs on.
pub struct Client {
    runtime: tokio::runtime::Runtime,
    models: Arc<Models>,
}

impl Client {
    /// Control-plane access (models, login, check_auth, catalogs).
    pub fn models(&self) -> &Models {
        &self.models
    }
}

/// Creates a client from scalar configuration with the built-in providers.
pub fn create_client(config: ClientConfig) -> Result<Client, NativeError> {
    let (http, bedrock) = native_transport()?;
    create_client_with_transport(config, http, bedrock)
}

/// Creates a client from a JSON object with the same fields as [`ClientConfig`].
pub fn create_client_from_json(json: &str) -> Result<Client, NativeError> {
    create_client(serde_json::from_str(json).map_err(|e| NativeError::Config(e.to_string()))?)
}

/// Rust-only injection seam; not part of the foreign surface (`alef(skip)`).
pub fn create_client_with_transport(
    config: ClientConfig,
    http: Arc<dyn HttpTransport>,
    bedrock: Arc<dyn BedrockSigner>,
) -> Result<Client, NativeError> {
    let providers = builtin_providers(BuiltinProviderInputs {
        http,
        bedrock,
        environment: config.environment,
    })
    .map_err(|e| NativeError::Client(e.to_string()))?;
    let mut builder = Models::builder();
    for provider in providers {
        builder = builder.provider(provider);
    }
    if let Some(path) = config.auth_store_path {
        builder = builder.credential_store(Arc::new(FileCredentialStore::new(path)));
    }
    let runtime = tokio::runtime::Runtime::new().map_err(|e| NativeError::Client(e.to_string()))?;
    let models = builder
        .build()
        .map_err(|e| NativeError::Client(e.to_string()))?;
    Ok(Client {
        runtime,
        models: Arc::new(models),
    })
}

/// Creates an idle agent on the client's runtime. `model` is `provider/model-id`.
pub fn create_agent(
    client: &Client,
    system_prompt: &str,
    model: &str,
    reasoning: Option<&str>,
) -> Result<Agent, NativeError> {
    let state = AgentState::new(
        system_prompt,
        parse_model(model)?,
        parse_reasoning(reasoning)?,
    );
    let core = agentprism_core::Agent::new(client.models.clone(), state, ToolRegistry::new())
        .map_err(|e| NativeError::Agent(e.to_string()))?;
    let _guard = client.runtime.enter();
    Agent::new(core).map_err(|e| NativeError::Agent(e.to_string()))
}

/// Restores an agent from a serialized [`AgentSnapshot`].
pub fn restore_agent(client: &Client, snapshot_json: &str) -> Result<Agent, NativeError> {
    let snapshot: AgentSnapshot =
        serde_json::from_str(snapshot_json).map_err(|e| NativeError::Config(e.to_string()))?;
    let core = agentprism_core::Agent::restore(
        snapshot,
        client.models.clone(),
        &|model: &ModelRef| client.models.model(model).is_some(),
        ToolRegistry::new(),
        &CustomRecordKinds::new(),
    )
    .map_err(|e| NativeError::Agent(e.to_string()))?;
    let _guard = client.runtime.enter();
    Agent::new(core).map_err(|e| NativeError::Agent(e.to_string()))
}

/// Stable construction-time error for foreign consumers.
#[derive(Clone, Debug)]
pub enum NativeError {
    /// Invalid configuration or serialized input.
    Config(String),
    /// Client assembly failed.
    Client(String),
    /// Agent construction or restore failed.
    Agent(String),
    /// The production HTTP transport crate has not landed yet (F14/F15).
    TransportUnavailable(String),
}

impl std::fmt::Display for NativeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (kind, message) = match self {
            Self::Config(m) => ("config", m),
            Self::Client(m) => ("client", m),
            Self::Agent(m) => ("agent", m),
            Self::TransportUnavailable(m) => ("transport unavailable", m),
        };
        write!(f, "{kind}: {message}")
    }
}

impl std::error::Error for NativeError {}

fn native_transport() -> Result<NativeTransports, NativeError> {
    Err(NativeError::TransportUnavailable(
        "agentprism-transport-reqwest is the designated production transport and has not landed"
            .into(),
    ))
}

fn parse_model(value: &str) -> Result<ModelRef, NativeError> {
    let (provider, model) = value
        .split_once('/')
        .ok_or_else(|| NativeError::Config(format!("model must be provider/model-id: {value}")))?;
    Ok(ModelRef {
        provider: provider.into(),
        model: model.into(),
    })
}

fn parse_reasoning(value: Option<&str>) -> Result<ReasoningLevel, NativeError> {
    Ok(match value {
        None | Some("off") => ReasoningLevel::Off,
        Some("minimal") => ReasoningLevel::Minimal,
        Some("low") => ReasoningLevel::Low,
        Some("medium") => ReasoningLevel::Medium,
        Some("high") => ReasoningLevel::High,
        Some("xhigh") => ReasoningLevel::Xhigh,
        Some(other) => {
            return Err(NativeError::Config(format!(
                "unknown reasoning level: {other}"
            )));
        }
    })
}
