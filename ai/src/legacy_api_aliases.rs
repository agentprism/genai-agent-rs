#[deprecated(note = "use api::anthropic_messages::stream")]
pub use crate::api::anthropic_messages::stream as stream_anthropic;
#[deprecated(note = "use api::anthropic_messages::stream_simple")]
pub use crate::api::anthropic_messages::stream_simple as stream_simple_anthropic;

#[deprecated(note = "use api::google_generative_ai::stream")]
pub use crate::api::google_generative_ai::stream as stream_google;
#[deprecated(note = "use api::google_generative_ai::stream_simple")]
pub use crate::api::google_generative_ai::stream_simple as stream_simple_google;

#[deprecated(note = "use api::google_vertex::stream")]
pub use crate::api::google_vertex::stream as stream_google_vertex;
#[deprecated(note = "use api::google_vertex::stream_simple")]
pub use crate::api::google_vertex::stream_simple as stream_simple_google_vertex;

#[deprecated(note = "use api::openai_codex_responses::stream")]
pub use crate::api::openai_codex_responses::stream as stream_open_ai_codex_responses;
#[deprecated(note = "use api::openai_codex_responses::stream_simple")]
pub use crate::api::openai_codex_responses::stream_simple as stream_simple_open_ai_codex_responses;

#[deprecated(note = "use api::openai_completions::stream")]
pub use crate::api::openai_completions::stream as stream_open_ai_completions;
#[deprecated(note = "use api::openai_completions::stream_simple")]
pub use crate::api::openai_completions::stream_simple as stream_simple_open_ai_completions;

#[deprecated(note = "use api::openai_responses::stream")]
pub use crate::api::openai_responses::stream as stream_open_ai_responses;
#[deprecated(note = "use api::openai_responses::stream_simple")]
pub use crate::api::openai_responses::stream_simple as stream_simple_open_ai_responses;

#[cfg(test)]
mod tests {
    use super::*;

    /// Ports pi `src/legacy-api-aliases.ts:19-108`; owner-ruling exclusions remove
    /// only the Azure Responses and Mistral Conversations alias pairs.
    #[test]
    #[allow(deprecated)]
    fn every_in_scope_alias_has_the_direct_api_signature() {
        let _ = stream_anthropic;
        let _ = stream_simple_anthropic;
        let _ = stream_google;
        let _ = stream_simple_google;
        let _ = stream_google_vertex;
        let _ = stream_simple_google_vertex;
        let _ = stream_open_ai_codex_responses;
        let _ = stream_simple_open_ai_codex_responses;
        let _ = stream_open_ai_completions;
        let _ = stream_simple_open_ai_completions;
        let _ = stream_open_ai_responses;
        let _ = stream_simple_open_ai_responses;
    }
}
