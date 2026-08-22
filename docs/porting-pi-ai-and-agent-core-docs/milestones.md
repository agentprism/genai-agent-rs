# Milestone closeouts

## 2026-08-22 — M1: contracts and fake runtime

M1 established the `pi-ai` canonical contracts, lossless replay-aware streaming and
assembly, the Send and Local runtime seams, portable cancellation, a hermetic
`ScriptedRuntime`, and the pinned parity manifest/checker.

### Approved packages

- M1.1 — canonical data model, replay envelope, descriptors, usage/cost, and
  finish/error types — `3fcf0abb6bb5b460a4af3c74a0d1d0b59bd92d48`
- M1.2 — streaming protocol, `AssistantAssembler`, `AssistantStream`, and replay
  invariants — `7531af4d71ef3a97dbc8bbfc6aeac16ce564fa41`
- M1.3 — Send and Local `ModelRuntime`, portable `CancellationToken`, model
  requests/options, and `ScriptedRuntime` —
  `b8b74db29e08d3bf73e7005d6723ff8485d17c1e`
- M1.4 — parity manifest, checker, pinned inventory, and CI wiring —
  `6b8b76dba22667a855d6fb4fea14c9daf09e124c`

### Architecture v2 Part 2 §10 conformance now passing

48 exact §10 conformance test names pass.

§10.1 stream conformance (14):

```text
stream_start_precedes_content
stream_exactly_one_terminal
stream_no_event_after_terminal
stream_failure_is_terminal_message
stream_cancellation_is_terminal_message
stream_partial_identity_is_stable
stream_response_id_is_preserved
stream_response_model_is_preserved
stream_usage_is_cumulative
stream_tool_json_scratch_not_persisted
stream_binary_scratch_not_persisted
stream_missing_provider_terminal_fails
stream_error_sanitizes_secrets
stream_unicode_matches_pi
```

§10.2 opaque replay conformance (27):

```text
anthropic_signature_fragments_append_in_order
anthropic_signature_survives_message_round_trip
anthropic_failed_partial_signature_is_not_replayed
openai_chat_reasoning_details_preserve_array_order
openai_chat_reasoning_details_survive_round_trip
openai_chat_incomplete_reasoning_detail_is_not_replayed
responses_response_id_survives_round_trip
responses_reasoning_item_preserves_full_json
responses_reasoning_encrypted_content_survives
responses_output_items_preserve_global_order
responses_text_item_id_survives
responses_text_phase_survives
responses_function_call_call_id_survives
responses_function_call_item_id_survives
responses_function_call_namespace_survives
responses_incomplete_output_item_is_not_replayed
bedrock_redacted_chunks_concatenate_as_bytes
bedrock_redacted_bytes_survive_json_round_trip
bedrock_partial_redacted_payload_is_not_replayed
google_thought_flag_not_signature_defines_thinking
google_text_part_signature_stays_on_text_part
google_thinking_part_signature_stays_on_thinking_part
google_tool_call_signature_stays_on_function_call
google_empty_signed_text_part_is_retained
google_empty_signed_thinking_part_is_retained
google_stream_omission_does_not_clear_prior_signature
google_signature_never_moves_between_parts
```

§10.5 simple lowering conformance (5):

```text
simple_typed_and_erased_patch_conflict
simple_unknown_api_patch_rejected
reasoning_xhigh_clamps_in_pi_mode
reasoning_xhigh_rejects_in_strict_mode
thinking_budget_defaults_match_pi
```

§10.7 catalog and auth conformance (2):

```text
catalog_unknown_extensions_round_trip
auth_provider_extra_fields_round_trip
```

### Parity manifest coverage

- Pinned upstream test files mapped: 159/159.
- Status-bearing mappings: 184 total — 15 `semantic-parity`, 10
  `deliberate-divergence`, and 159 `planned`.
- Future named conformance tests: 27 `planned_test` entries, each assigned to a
  milestone.

### Architecture correction notes

None. M1.1 through M1.4 added no `> Correction:` notes to either architecture
document.
