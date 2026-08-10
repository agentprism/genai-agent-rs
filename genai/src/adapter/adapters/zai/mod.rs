//! ZAI API Documentation
//! API Documentation:     <https://api.z.ai>
//! Model Names:           GLM series models
//! Pricing:               <https://api.z.ai/pricing>
//!
//! ## Dual Endpoint Support
//!
//! ZAI supports two different API endpoints using the ServiceTargetResolver pattern:
//!
//! ### Regular API (Credit-based) (default for those models or with `zai::` namespace)
//! - Endpoint: `<https://api.z.ai/api/paas/v4/>`
//! - Models: `glm-4.6`, `glm-4.5`, etc.
//! - Usage: Standard API calls billed per token
//!
//! ### Coding Plan (Subscription-based only with the `zai_coding::` namepace)  
//! - Endpoint: `<https://api.z.ai/api/coding/paas/v4/>`
//! - Models: `zai_coding::glm-4.6`, `zai_coding::glm-4.5`, etc.
//! - Usage: Fixed monthly subscription for coding tasks
//!
//! ## For example
//!
//! ```rust,no_run
//! use genai::Client;
//! use genai::chat::ChatRequest;
//!
//! # async fn demo(target_resolver: genai::resolver::ServiceTargetResolver)
//! #     -> Result<(), Box<dyn std::error::Error>> {
//! let client = Client::builder().with_service_target_resolver(target_resolver).build();
//! let chat_request = ChatRequest::default();
//!
//! // Regular API (credit-based) — default for these models, or via the `zai::` namespace
//! let _response = client.exec_chat("glm-4.6", chat_request.clone(), None).await?;
//! let _response = client.exec_chat("zai::glm-4.6", chat_request.clone(), None).await?;
//!
//! // Coding plan (subscription) — the `zai_coding::` namespace
//! let _response = client.exec_chat("zai_coding::glm-4.6", chat_request, None).await?;
//! # Ok(())
//! # }
//! ```
//!
//! See `examples/c07-zai-dual-endpoints.rs` for a complete working example.

// region:    --- Modules

mod adapter_impl;

pub use adapter_impl::*;

// endregion: --- Modules
