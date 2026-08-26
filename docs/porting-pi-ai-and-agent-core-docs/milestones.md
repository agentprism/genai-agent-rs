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

## 2026-08-22 — M2: agent loop

M2 established the `pi-agent-core` state and event contracts, the Send and Local
run state machines, control queues, continue/retry/reset behavior, executable and
typed tools, deterministic tool scheduling, cancellation joining, and the
context, projection, tool, and turn policy seams. The implementation is driven
entirely by `ScriptedRuntime`; it does not talk to providers.

### Approved packages

- M2.1 — agent state, records, events, outcomes, snapshots, replay, and restore —
  `4b8e50a141bdcc94de97eb3470f6fab45e135508`
- M2.2 — run state machine, phases, queues, continue/retry/reset, and the Tokio
  actor facade — `f821a47c32f392ea6506a74b7125f072c4ac9c48`
- M2.3 — dynamic and typed tools, registry, validation, scheduling, and
  cancellation joining — `a0125dc778305eadd5bde9da5b32236a46ec9dae`
- M2.4 — `ContextPolicy`, `MessageProjector`, `ToolPolicy`, `TurnPolicy`, and
  `PreparedContext` — `6c4b688ee810cb34b1c2b81ecaacaa41046b2ab7`

### Architecture v2 Part 2 §10 conformance now passing

81 new exact §10.9 conformance test names pass, bringing the cumulative exact
§10 total to 129.

§10.9 lifecycle (13):

```text
agent_prompt_text_event_sequence
agent_prompt_message_event_sequence
agent_prompt_message_batch_event_sequence
agent_continue_event_sequence
agent_prompt_without_tools
agent_prompt_with_one_tool
agent_prompt_with_multiple_tools
agent_continue_without_tools
agent_continue_with_tools
agent_run_finished_is_final_event
agent_low_level_stream_is_observational
agent_handle_event_sinks_are_barriers
agent_wait_for_idle_includes_run_finished_sinks
```

§10.9 failure and cancellation (11):

```text
agent_failed_assistant_is_committed
agent_cancelled_assistant_is_committed
agent_partial_content_survives_failure
agent_partial_usage_survives_failure
agent_failed_turn_has_turn_finished
agent_failed_turn_has_run_finished
agent_no_tools_execute_after_failed_assistant
agent_continue_rejects_assistant_tail
agent_continue_drains_steering_before_rejecting_assistant_tail
agent_continue_drains_followup_after_steering
agent_retry_last_turn_reuses_last_valid_request_boundary
```

§10.9 context phases (9):

```text
agent_transform_context_runs_before_projector
agent_projector_runs_once_per_model_turn
agent_context_policy_receives_cancellation
agent_prepare_next_turn_runs_after_turn_finished
agent_prepare_next_turn_can_replace_context
agent_prepare_next_turn_can_replace_model
agent_prepare_next_turn_can_replace_reasoning
agent_should_stop_runs_after_prepare_next_turn
agent_should_stop_precedes_queue_poll
```

§10.9 tools (25):

```text
tool_unknown_name_becomes_error_result
tool_prepare_arguments_precedes_validation
tool_validation_precedes_before_hook
tool_before_hook_can_block
tool_before_hook_can_terminate
tool_execution_error_becomes_error_result
tool_updates_precede_tool_finished
tool_late_updates_are_ignored
tool_after_hook_precedes_tool_finished
tool_after_hook_can_replace_content
tool_after_hook_can_replace_details
tool_after_hook_can_replace_usage
tool_after_hook_can_change_error_state
tool_after_hook_can_terminate
tool_any_sequential_tool_forces_sequential_batch
tool_parallel_preflight_is_source_order
tool_parallel_completion_events_are_completion_order
tool_parallel_result_messages_are_source_order
tool_parallel_turn_results_are_source_order
tool_batch_terminates_only_when_all_results_terminate
tool_length_truncated_calls_are_never_executed
tool_length_truncated_calls_each_receive_error_result
tool_cancellation_stops_new_sequential_calls
tool_cancellation_joins_running_parallel_calls
tool_no_process_or_file_mutation_after_run_finished
```

§10.9 queues (13):

```text
queue_steering_polled_at_run_start
queue_steering_not_polled_between_tools
queue_steering_polled_after_completed_turn
queue_steering_polled_after_prepare_next_turn
queue_steering_not_polled_when_should_stop_returns_true
queue_followup_polled_only_when_agent_would_stop
queue_one_mode_drains_one
queue_all_mode_drains_all
queue_ingress_order_is_stable
queue_clear_steering
queue_clear_followup
queue_clear_all
queue_concurrent_producers_use_control_handle
```

§10.9 state management (10):

```text
agent_reset_rejects_while_active
agent_reset_clears_transcript
agent_reset_clears_partial_state
agent_reset_clears_pending_tool_calls
agent_reset_clears_error
agent_reset_clears_queues
agent_reset_preserves_model
agent_reset_preserves_system_prompt
agent_reset_preserves_tools
agent_reset_preserves_runtime_and_policies
```

### Parity manifest coverage

- Pinned upstream test files mapped: 159/159.
- Status-bearing mappings: 184 total — 17 `semantic-parity`, 10
  `deliberate-divergence`, and 157 `planned`.
- Future named conformance tests: 28 `planned_test` entries, each assigned to a
  milestone.

### Architecture correction notes

M2 added two corrections to Architecture v2 Part 2 §8.2:

- M2.2 corrected prompt-run ordering: `InitialQueuePoll` follows `RunStarted`,
  `TurnStarted`, and initial prompt commitment, while drained steering records
  are injected before `PrepareContext`.
- M2.3 corrected tool-result ordering: parallel batches defer source-ordered
  result-message commitment until joined executions settle, whereas sequential
  and length-truncated synthesis paths emit each tool end/result lifecycle before
  starting the next call.

## 2026-08-23 — M3: `Models` control plane

M3 established provider/API composition, the `Models` registry and request
pipeline, middleware and retry contracts, immutable catalog snapshots and
refresh publication, credential leases and authentication interactions,
API-family lowering, and structured cross-provider handoff. `Models` is the
full provider/model/auth/catalog control plane and implements the narrow
`ModelRuntime` capability consumed by `pi-agent-core`.

### Approved packages

- M3.1 — provider composition, `Models` registry/router, request pipeline,
  middleware, and retry — `089f92a71b2162bf61ffa32487960dc54f443392`
- M3.2 — catalog sources, stores, overrides, layers, and
  refresh/persist/publish — `a25ead1d103e8562365b94fb9f63cbf64356aed2`
- M3.3 — auth resolvers, credential store and lease, interactions, redirect
  receivers, and device-code flow —
  `d69203cd8096447e3cb4b7b7bd8cf751ad15efb5`
- M3.4 — `ApiFamily`, erased handlers, common planning, and handoff policy and
  report — `8cdaf5ef7173a45a68b94daf3ed84c1e274a5b8a`

### Architecture v2 Part 2 §10 conformance now passing

105 new exact §10 conformance test names pass, bringing the cumulative exact
§10 total to 234. Seven additional M3 tests re-exercise exact §10 names already
recorded by M1, so the M3 packages directly exercise 112 exact §10 names.

§10.3 retry conformance (20 new):

```text
retry_x_should_retry_true_overrides_status
retry_x_should_retry_false_overrides_status
retry_transport_failure_without_status
retry_http_408
retry_http_409
retry_http_429
retry_http_500_through_599
retry_non_retryable_4xx
retry_after_ms_precedes_retry_after
retry_after_accepts_decimal_seconds
retry_after_accepts_http_date
retry_server_delay_over_max_fails_immediately
retry_zero_max_delay_disables_cap
retry_exponential_sequence_matches_pi
retry_jitter_range_matches_pi
retry_cancellation_before_attempt
retry_cancellation_during_request
retry_cancellation_during_backoff
retry_never_restarts_after_semantic_event
retry_fresh_transport_attempt_number
```

§10.4 middleware conformance (18 new):

```text
headers_merge_case_insensitively
headers_auth_before_model
headers_model_before_explicit
headers_explicit_before_transform
headers_transform_can_delete_default
headers_transform_runs_once
headers_transform_not_forwarded_to_provider_options
payload_in_place_mutation_is_retained
payload_replacement_is_retained
payload_transforms_run_in_registration_order
payload_transform_runs_once_per_logical_request
attempt_middleware_runs_per_retry
response_observer_runs_before_body_consumption
response_observer_runs_for_retry_responses
injected_http_transport_receives_final_request
bedrock_custom_headers_are_inserted_before_signing
bedrock_reserved_headers_are_suppressed
bedrock_response_observer_receives_raw_headers
```

§10.5 simple-lowering conformance (10 new):

```text
simple_context_reserves_4096_tokens
simple_context_clamp_never_returns_zero
simple_max_output_respects_model_limit
simple_model_sampling_defaults_apply
simple_request_sampling_overrides_model_defaults
simple_api_patch_overrides_common_simple_field
reasoning_explicit_unsupported_is_not_treated_as_missing
thinking_budget_reserves_1024_answer_tokens
thinking_budget_expands_explicit_answer_cap
thinking_budget_respects_model_max_output
```

§10.6 handoff conformance (21 new):

```text
handoff_null_content_normalized
handoff_nonvision_user_image_replaced
handoff_nonvision_tool_image_replaced
handoff_adjacent_image_placeholders_collapsed
handoff_failed_assistant_omitted
handoff_aborted_assistant_omitted
handoff_redacted_thinking_retained_exact_model
handoff_redacted_thinking_dropped_cross_model
handoff_signed_empty_thinking_retained_exact_model
handoff_visible_thinking_becomes_plain_text_in_pi_mode
handoff_visible_thinking_becomes_tagged_text_when_configured
handoff_text_signature_dropped_cross_model
handoff_tool_signature_dropped_cross_model
handoff_tool_id_normalized
handoff_matching_tool_result_id_rewritten
handoff_tool_id_collision_gets_stable_hash
handoff_missing_tool_result_synthesized
handoff_existing_tool_result_not_duplicated
handoff_multiple_missing_results_preserve_source_order
handoff_loss_report_contains_every_drop
handoff_strict_mode_rejects_lossy_projection
```

§10.7 catalog conformance (14 new):

```text
catalog_reads_last_published_snapshot_synchronously
catalog_static_refresh_is_noop
catalog_restore_precedes_auth_resolution
catalog_restore_precedes_network
catalog_network_refresh_is_best_effort_per_provider
catalog_superseded_refresh_cannot_publish
catalog_persist_precedes_publish
catalog_failed_persist_keeps_old_snapshot
catalog_reader_never_sees_partial_candidate
catalog_host_override_applies_after_dynamic_snapshot
catalog_removed_override_reveals_provider_value
catalog_raw_snapshot_does_not_contain_flattened_override
catalog_typed_compat_mismatch_is_rejected
catalog_runtime_override_has_highest_precedence
```

§10.7 authentication conformance (22 new):

```text
auth_explicit_request_value_wins
auth_stored_credential_owns_provider
auth_environment_used_only_without_stored_credential
auth_failed_oauth_refresh_never_falls_back_to_env
auth_oauth_refresh_is_serialized
auth_login_persists_under_modify
auth_list_never_resolves_secrets
auth_text_prompt
auth_secret_prompt
auth_select_returns_option_id
auth_manual_code_can_be_cancelled_by_callback
auth_device_default_interval_is_five_seconds
auth_device_interval_minimum_is_one_second
auth_device_slow_down_adds_five_seconds
auth_device_server_interval_wins
auth_device_deadline_is_enforced
auth_device_poll_is_cancellable
auth_pkce_state_is_validated
auth_callback_and_manual_first_valid_wins
auth_late_losing_response_is_superseded
auth_mobile_custom_scheme_flow
auth_mobile_unsupported_fixed_loopback_is_explicit
```

The seven exact names re-exercised by M3, and therefore excluded from the new
counts above, are:

```text
catalog_unknown_extensions_round_trip
auth_provider_extra_fields_round_trip
simple_typed_and_erased_patch_conflict
simple_unknown_api_patch_rejected
reasoning_xhigh_clamps_in_pi_mode
reasoning_xhigh_rejects_in_strict_mode
thinking_budget_defaults_match_pi
```

### Parity manifest coverage

- Pinned upstream test files mapped: 159/159 — 7 `semantic-parity`, 0
  `deliberate-divergence`, and 152 `planned`.
- Status-bearing mappings: 193 total — 31 `semantic-parity`, 10
  `deliberate-divergence`, and 152 `planned`.
- Future named conformance tests: 28 `planned_test` entries, each assigned to a
  milestone.

### Architecture correction notes

M3 added nine corrections:

- M3.1 corrected Architecture v2 Part 2 §2.4: pinned Pi retries every numeric
  status greater than or equal to 500, not only HTTP 5xx statuses.
- M3.1 corrected Architecture v2 Part 2 §2.6: Bedrock silently suppresses
  reserved `x-amz-*`, `authorization`, and `host` headers at the pre-signing
  build step; it does not emit diagnostics naming the suppressed headers.
- M3.2 corrected Architecture v2 Part 2 §5.7: static and unknown providers are
  absent from refresh results, and errors from aborted or superseded provider
  generations are suppressed.
- M3.3 corrected Architecture v2 Part 1 §3.8: `Models.logout` deletes the stored
  credential and does not call provider-owned logout cleanup.
- M3.4 corrected Architecture v2 Part 2 §3.4: nonpositive context windows bypass
  estimation/clamping, while positive windows clamp only against remaining
  context and may allow an explicit request above the catalog model maximum.
- M3.4 corrected Architecture v2 Part 2 §3.4: Pi keeps `samplingParams`
  separate from named simple fields and overlays their merged values after the
  named OpenAI-family fields.
- M3.4 corrected Architecture v2 Part 2 §3.7: the Pi-mode reasoning clamp
  searches at and above the requested level before searching lower levels, then
  falls back to `off`.
- M3.4 corrected Architecture v2 Part 2 §4.3: a preceding successful
  assistant's pending calls are closed before a later failed/aborted assistant
  is omitted, without synthesizing results for calls inside the omitted turn.
- M3.4 corrected Architecture v2 Part 2 §4.3: tool-call ID normalization and
  the pass-wide old-to-new mapping apply under pinned Pi's exact cross-model,
  changed-ID, failed-turn, and later-result conditions before failed-turn
  omission.

## 2026-08-23 — M4: API/provider separation

M4 established the ordered, JavaScript-compatible wire writer and captured Pi
fixture corpus, then implemented OpenAI-compatible Chat Completions and
Anthropic Messages as independent API-family handlers with lowering, exact wire
encoding, stream decoding, replay assembly, and provider registrations. DeepSeek
and OpenRouter share the OpenAI-compatible family implementation; Anthropic uses
the Anthropic Messages family implementation.

### Approved packages

- M4.1 — ordered JSON wire writer and Pi fixture capture corpus —
  `e692c4cb6dd757339469e6e84a9614c432248111`
- M4.2 — OpenAI-compatible Chat Completions lowering, encoder, decoder, replay
  items, and the DeepSeek/OpenRouter provider registrations sharing that family —
  `ece3d55ef385e51587e548ae06355f1833d89d65`
- M4.3 — Anthropic Messages lowering, encoder, decoder, signature replay items,
  and the Anthropic provider registration —
  `b2ffa6ff6d47bc71e9dc32c295fefb3176b6d245`

### Architecture v2 Part 2 §10 conformance now passing

27 new exact §10 conformance test names pass, bringing the cumulative exact
§10 total to 261. Ten additional M4 tests re-exercise exact §10 names already
recorded by M1 or M3, so the M4 packages directly exercise 37 exact §10 names.

§10.2 opaque replay conformance (11 new):

```text
anthropic_turn_two_replays_exact_signature
anthropic_redacted_thinking_replays_exact_data
anthropic_unsigned_thinking_falls_back_to_text
anthropic_empty_signature_respects_compat
anthropic_signature_never_crosses_model_boundary
openai_chat_reasoning_field_name_is_preserved
openai_chat_reasoning_details_replay_exact_json
openai_chat_block_signature_precedes_legacy_tool_signature
openai_chat_legacy_tool_signature_imports_as_replay_item
openai_chat_thinking_as_text_compat
openai_chat_reasoning_content_required_compat
```

§10.5 simple-lowering conformance (11 new):

```text
anthropic_adaptive_uses_effort
anthropic_budget_model_uses_budget_tokens
anthropic_temperature_omitted_while_thinking
anthropic_temperature_omitted_when_model_disallows_it
anthropic_disabled_thinking_respects_compat
openai_compat_is_detected_from_effective_base_url
openai_model_compat_overrides_url_detection
openai_max_tokens_field_matches_compat
openai_reasoning_format_matches_compat
openai_thinking_budget_field_matches_compat
openai_sampling_params_merge_after_named_fields
```

§10.8 golden provider request bodies (5 new):

```text
wire_openai_completions_pi_exact
wire_anthropic_messages_pi_exact
openai_chat_reasoning_details_turn_two_pi_exact
anthropic_signed_thinking_turn_two_pi_exact
anthropic_redacted_thinking_turn_two_pi_exact
```

The ten exact names re-exercised by M4, and therefore excluded from the new
counts above, are:

```text
stream_failure_is_terminal_message
stream_cancellation_is_terminal_message
stream_response_id_is_preserved
stream_response_model_is_preserved
anthropic_signature_fragments_append_in_order
anthropic_signature_survives_message_round_trip
anthropic_failed_partial_signature_is_not_replayed
headers_model_before_explicit
headers_explicit_before_transform
headers_transform_can_delete_default
```

### Parity manifest coverage

- Pinned upstream test files mapped: 159/159 — 21 `semantic-parity`, 0
  `deliberate-divergence`, and 138 `planned`.
- Status-bearing mappings: 203 total — 55 `semantic-parity`, 10
  `deliberate-divergence`, and 138 `planned`.
- Future named conformance tests: 14 `planned_test` entries, each assigned to a
  milestone.
- Rust test inventory discovered by the parity checker: 417 unique tests.

### Architecture correction notes

M4 added ten corrections, all to Architecture v2 Part 2:

- M4.2 corrected §1.3: OpenAI Completions preserves the raw provider stop
  reason even when finish-reason mapping produces a failed assistant.
- M4.3 corrected §1.4: Anthropic redacted replay uses its redacted payload
  independently of ordinary signatures, and whitespace-only ordinary
  signatures are empty for compatibility decisions.
- M4.2 corrected §2.4: request-scoped canonical context remains available to
  OpenAI Completions decoding so custom grammar-tool deltas close with the
  configured input-property name.
- M4.3 corrected §3.5: full Anthropic options distinguish omitted thinking from
  explicit disablement, retain native tool choice, and carry the interleaved
  thinking transport preference.
- M4.2 corrected §3.6: full OpenAI Completions tool choice uses the complete
  optional native domain while the provider-neutral simple choice remains
  `auto` or `none`.
- M4.2 corrected §3.6: OpenAI reasoning lowering is a format mode plus an
  independent token budget, including Baseten, Ant Ling, and ordered named
  temperature/sampling overlay behavior.
- M4.2 corrected §3.6: OpenAI compatibility detection also depends on provider
  identity and, for OpenRouter, model-id prefixes; built-in descriptors
  materialize values not inferable from the effective URL.
- M4.3 corrected §5.1: `AnthropicEffort` includes `Minimal` because Pi accepts
  string-valued thinking-level mappings and the captured minimal fixture emits
  `"minimal"`.
- M4.2 corrected §5.2: pricing and cost fixed-point integers are signed so Pi's
  published negative OpenRouter rates survive and calculate exactly.
- M4.3 corrected §5.2: usage retains Anthropic's one-hour cache-write subset and
  prices only that subset at twice the input rate.

## 2026-08-23 — M5: persistent credentials and FFI

M5 established durable file-backed credential leases with serialized OAuth
refresh and a versioned persisted credential format, then added the `pi-ffi`
binding facade with opaque handles, versioned lossless event envelopes,
cancellation, an explicit authentication-session state machine, a C ABI, and a
generated Swift binding target.

### Approved packages

- M5.1 — file-backed credential leases, OAuth refresh locking, and the persisted
  credential format — `dec709e51a72cc360dc4dfa6522666cd7edf6ade`
- M5.2 — `pi-ffi` opaque handles, versioned event envelopes, cancellation, auth
  session state machine, C ABI, and generated Swift bindings —
  `3d393c249a210e17cd06ed2a84b6a63ac04bc777`

### Architecture v2 Part 2 §10 conformance now passing

M5 adds no previously absent exact §10 conformance name, so the cumulative exact
§10 total remains 261. The M5 packages directly re-exercise nine exact §10 names
against the new persistent store and FFI boundaries.

§10.7 authentication conformance (5 re-exercised):

```text
auth_oauth_refresh_is_serialized
auth_failed_oauth_refresh_never_falls_back_to_env
auth_login_persists_under_modify
auth_callback_and_manual_first_valid_wins
auth_late_losing_response_is_superseded
```

§10.9 lifecycle conformance (3 re-exercised):

```text
agent_prompt_text_event_sequence
agent_run_finished_is_final_event
agent_handle_event_sinks_are_barriers
```

§10.9 failure and cancellation conformance (1 re-exercised):

```text
agent_cancelled_assistant_is_committed
```

M5 also adds boundary-specific coverage for persisted provider extras, persisted
schema rejection, the Local credential-store adapter, lossless C event envelopes,
device-code and shared callback/manual auth sessions, exact FFI challenge schema,
and invalid C-ABI argument handling.

### Parity manifest coverage

- Pinned upstream test files mapped: 159/159 — 21 `semantic-parity`, 0
  `deliberate-divergence`, and 138 `planned`.
- Status-bearing mappings: 208 total — 60 `semantic-parity`, 10
  `deliberate-divergence`, and 138 `planned`.
- Future named conformance tests: 14 `planned_test` entries, each assigned to a
  milestone.

### Architecture correction notes

None. M5.1 and M5.2 added no `> Correction:` notes to either architecture
document.

## 2026-08-25 — M6: remaining API families and providers

M6 completed the provider/API implementation milestone: OpenAI Responses,
OpenAI Codex Responses, Google Generative AI, Google Vertex, Bedrock Converse
Stream, Azure OpenAI Responses, Mistral Conversations, and pi-messages now join
the OpenAI Completions and Anthropic Messages families from M4. It also added
Cloudflare, Radius, the remaining pinned provider registrations,
credential-scoped availability, the context-overflow classifier, and the
`pi-ai-providers-all` aggregator.

### Approved packages

- M6.1 — OpenAI Responses and OpenAI Codex Responses families and providers —
  `4366b970c9dac8e7ca168bdd4abcde387c8b214c`
- M6.2 — Google Generative AI and Google Vertex families and providers —
  `6253bde65be23b2de2c9f2d3cf75bbe89fede33a`
- M6.3 — Bedrock Converse Stream family and provider —
  `a24cf5bfe91c8cdf486895ebf2f61cb05194114f`
- M6.4 — Azure OpenAI Responses, Mistral Conversations, and pi-messages
  families; Cloudflare, Radius, and every remaining provider;
  credential-scoped availability; context-overflow classification; and
  `pi-ai-providers-all` — `0e10c1d86485ef5213fa02ed1f014fd104513377`

### Architecture v2 Part 2 §10 conformance now passing

21 new exact §10 conformance test names pass, bringing the cumulative exact
§10 total to 282.

§10.2 opaque replay conformance (9 new):

```text
responses_different_model_drops_paired_item_id
responses_foreign_function_item_id_is_normalized
responses_turn_two_input_items_match_pi_order
bedrock_turn_two_replays_redacted_content_bytes
bedrock_signed_reasoning_replays_text_and_signature
bedrock_missing_required_signature_falls_back_to_text
bedrock_non_anthropic_model_omits_reasoning_signature
google_invalid_base64_signature_is_dropped
google_signature_requires_same_provider_and_model
```

§10.8 golden provider request bodies (12 new):

```text
wire_openai_responses_pi_exact
wire_openai_codex_responses_pi_exact
wire_azure_openai_responses_pi_exact
wire_google_generative_ai_pi_exact
wire_google_vertex_pi_exact
wire_bedrock_converse_stream_pi_exact
wire_mistral_conversations_pi_exact
wire_pi_messages_pi_exact
openai_responses_encrypted_reasoning_turn_two_pi_exact
bedrock_redacted_reasoning_turn_two_pi_exact
google_tool_thought_signature_turn_two_pi_exact
google_empty_signed_part_turn_two_pi_exact
```

Together with M4, all ten §10.8 API-family wire tests and all seven required
two-turn replay goldens pass after event assembly and a persistence round-trip.

### Parity manifest coverage

- Pinned upstream test files mapped: 159/159 — 62 `semantic-parity`, 0
  `deliberate-divergence`, and 97 `planned`.
- Status-bearing mappings: 244 total — 137 `semantic-parity`, 10
  `deliberate-divergence`, and 97 `planned`.
- Future named conformance tests: 1 `planned_test` entry.
- Rust test inventory discovered by the parity checker: 694 unique tests.

### Architecture correction notes

M6 added 34 corrections: one to Architecture v2 Part 1 and 33 to Architecture
v2 Part 2.

- M6.1 corrected Part 2 §1.2: assistant messages and snapshots retain calculated
  monetary cost separately from token usage, and agent aggregation is limited
  to same-currency, fully known costs.
- M6.1 corrected Part 2 §1.3: Responses can replace streamed function arguments
  with a non-prefix authoritative terminal value, requiring
  `ToolArgumentsReplaced`.
- M6.1 corrected Part 2 §1.6: Responses turn-two encoding walks canonical blocks
  in order and consults surviving replay identities in place rather than
  emitting all replay records first.
- M6.1 corrected Part 2 §1.6: terminal Responses text and reasoning may replace
  streamed values; Codex also retains `end_turn` and persisted transport-failure
  diagnostics.
- M6.1 corrected Part 2 §1.6: cached Codex WebSocket continuation is derived by
  canonical re-encoding of the assembled assistant, not by copying terminal
  `response.output`.
- M6.1 corrected Part 2 §1.6: Codex maps `response.failed` and top-level `error`
  through its family-specific nested-message behavior without retaining
  `response.status` as the raw stop reason.
- M6.1 corrected Part 2 §1.6: Responses finalization falls back to streamed
  reasoning, tool arguments, and custom input under Pi's exact omission and
  empty-value rules, and does not copy `response.model`.
- M6.1 corrected Part 2 §1.6: deferred function/custom-tool namespaces survive
  same-provider/API model changes while paired `fc_*` item IDs remain
  model-sensitive.
- M6.1 corrected Part 2 §1.6: any mapped Codex WebSocket event starts the stream,
  nested missing-continuation retry remains special, and WebSocket exchanges do
  not invoke the SSE response observer.
- M6.1 corrected Part 2 §1.6: fallback `msg_pi_*` counters advance only after a
  source message emits at least one wire item.
- M6.1 corrected Part 2 §1.6: Codex alone normalizes unknown terminal status to
  absent and discards an unterminated SSE EOF tail.
- M6.1 corrected Part 2 §1.6: every typed-session Codex WebSocket transport
  failure records sticky SSE fallback and clears cached continuation, including
  failures after semantic stream start.
- M6.1 corrected Part 2 §1.6: shared Responses accepts only `response.completed`
  and `response.incomplete` as successful terminals and ignores deltas for an
  output slot after its item-done event.
- M6.1 corrected Part 2 §1.6: Responses service-tier costs use Pi's exact `flex`,
  `priority`, and GPT-5.5 priority multipliers, including Codex's echoed-default
  resolution.
- M6.1 corrected Part 2 §2.4: Codex uses a distinct retry classifier and policy,
  including its status/body allowlists, quota exclusions, delay handling,
  terminal normalization, and unjittered one-second exponential backoff.
- M6.1 corrected Part 2 §2.4: API execution context retains original call options
  so Codex pricing can distinguish the requested service tier independently of
  payload middleware.
- M6.1 corrected Part 2 §2.6: Codex SSE auth, account, originator, user-agent,
  and protocol headers are final transport invariants after model/caller
  overlays.
- M6.1 corrected Part 2 §2.6: Codex cache/continuation affinity derives only from
  typed session options, while separately clamped protocol IDs are reasserted
  at the transport boundary.
- M6.1 corrected Part 2 §2.6: public Responses header-only authentication is
  eligible only from explicit option headers and must remain nonempty after
  final transforms.
- M6.1 corrected Part 2 §2.6: public Responses session-affinity headers follow
  model headers and precede explicit request headers for simple and full calls.
- M6.1 corrected Part 2 §3.4: Responses retains the clamped reasoning-level name
  and applies `thinkingLevelMap` exactly once in the full encoder, including the
  summary-only exception.
- M6.1 corrected Part 2 §6.1: Codex accepts a direct caller-supplied OAuth access
  token, derives its account ID from the JWT, and does not require stored refresh
  credentials on that path.
- M6.1 corrected Part 2 §6.1: Codex browser OAuth advertises and exchanges the
  registered localhost redirect URI while preserving Pi's callback/manual-code
  state-validation distinctions.
- M6.2 corrected Part 1 §3.9: usage retains provider `total_tokens`, and context
  planning prefers it only when nonzero before falling back to normalized
  components.
- M6.2 corrected Part 2 §2.6: Google Vertex full `project` and `location` options
  may establish request-scoped ADC/client scope before ordinary auth resolution.
- M6.2 corrected Part 2 §3.6: simple options preserve whether
  `thinking_budgets` was omitted because Gemini 2.5 defaults distinguish
  omission from an explicit default-valued map.
- M6.3 corrected Part 2 §1.7: a Bedrock redacted-content transition can discard
  an earlier signature replay item, requiring `ReplayItemDiscarded`.
- M6.3 corrected Part 2 §2.4: Bedrock encoding consumes request-scoped provider
  environment decisions through credential-derived invariant context with Pi's
  truthiness and precedence rules.
- M6.3 corrected Part 2 §2.6: Bedrock resolves proxy variables before client
  construction and selects an HTTP/1 proxy-capable handler when a proxy applies.
- M6.3 corrected Part 2 §2.6: the private auth-to-signer carrier is suppressed
  only while untouched; a logical-header overlay using its name is forwarded.
- M6.3 corrected Part 2 §2.6: the earlier architecture clause requiring Bedrock
  reserved-header suppression diagnostics is superseded; Pi suppresses those
  names silently.
- M6.4 corrected Part 2 §1.3: pi-messages `toolcall_end` authoritatively replaces
  streamed tool-call identity and arguments, requiring
  `ToolCallMetadataReplaced` as well as argument replacement.
- M6.4 corrected Part 2 §6.6: GitHub Copilot credentials retain
  `available_model_ids` for entitlement-scoped catalog filtering.
- M6.4 corrected Part 2 §6.6: GitHub Copilot credentials retain normalized
  `enterprise_url` independently of account identity for refresh and request
  authentication.
## 2026-08-25 — M7: sessions, environment, and native runtime

M7 established the native `pi-agent-session` protocol and reducer over an
immutable entry tree, lanes, operation records, recovery, and branching. It also
established the portable environment capability traits, their Tokio filesystem
and process implementation, process-tree termination semantics, and the native
Tokio actor facade. The session and environment/runtime crates remain outside
`pi-agent-core` as required by Architecture v2 Part 2 §§7 and 9.

### Approved packages

- M7.1 — `pi-agent-session`: entry tree, lanes, operation records, Send and Local
  storage traits, reducer, recovery, and branching —
  `8c9f3c6c0859757c7c02c0481e4cfe04a803d5bf`
- M7.2 — `pi-agent-env` and `pi-agent-runtime-tokio`: portable capability
  traits, Tokio filesystem and process execution, termination behavior, and
  actor facade — `aa3f90d635f7edac1a36c6084a1b3fcf4e281487`

### Architecture v2 Part 2 §10 conformance now passing

M7 adds 27 previously absent exact §10.10 conformance names. The final M7 tree
contains 306 exact §10 conformance names in total.

§10.10 reducer and session-tree conformance (16 new):

```text
session_sequence_starts_at_one
session_sequence_is_global_across_mutation_kinds
session_sequence_gap_is_corruption
session_entry_parent_must_exist
session_lane_head_moves_on_append
session_lane_can_move_to_ancestor
session_multiple_lanes_share_entry_tree
session_branch_scan_leaf_to_root
session_global_entry_query_sequence_order
session_fact_latest_value_wins
session_label_is_global_not_branch_scoped
session_stats_derive_from_usage_records
session_open_operation_detected
session_multiple_open_operations_is_corruption
session_operation_recovery_reconstructs_intent
session_reducer_replay_equals_live_state
```

§10.10 environment conformance (11 new):

```text
env_read_file
env_write_file
env_atomic_replace
env_process_stdout_stream
env_process_stderr_stream
env_process_exit_status
env_process_graceful_termination
env_process_forced_termination
env_process_tree_termination
env_stdio_grace_period
env_cancellation
```

M7 additionally adds architecture-specific coverage for lane-scoped operation,
queue, and tool identities; atomic storage rejection; repository branch/tree
forks; Send and Local storage object safety; large bidirectional process I/O;
non-tree termination; explicit unavailable process capability; and serial
processing of all nine Tokio actor commands.

### Parity manifest coverage

- Pinned upstream test files mapped: 159/159 — 57 `semantic-parity`, 0
  `deliberate-divergence`, and 102 `planned`.
- Status-bearing mappings: 235 total — 123 `semantic-parity`, 10
  `deliberate-divergence`, and 102 `planned`.
- Future named conformance tests: 1 `planned_test` entry, assigned to M3.4.

### Architecture correction notes

M7 added three corrections, all from M7.1 to Architecture v2 Part 2:

- §7.2 now records that Pi session usage records permit negative token and
  monetary adjustments; native response `Usage` remains unsigned, while the
  operation record carries separate cost and signed fixed-point adjustment
  fields without `f64`.
- §7.4 now records that replay retains unmatched operation-finished records and
  multiple unresolved starts for corruption diagnosis, while live in-memory and
  JSONL writers reject a second open operation on one lane.
- §7.5 now records that Pi branch forks accept only message-entry targets;
  custom, compaction, and other entry targets are rejected as
  `invalid_fork_target`, while whole-tree forks remain unrestricted.

M7.2 added no correction note.

## 2026-08-25 — M7 final closeout: deferred responses, sessions, environment, and native runtime

This final M7 record supersedes the earlier M7 entry for package enumeration and
coverage totals because the approved M7.0 deferred-response package landed after
the initial closeout. M7 established serializable deferred-response execution,
the native `pi-agent-session` protocol and reducer, portable environment
capabilities, their Tokio filesystem and process implementation, process-tree
termination semantics, and the native Tokio actor facade. These session and
environment/runtime crates remain outside `pi-agent-core` as required by
Architecture v2 Part 2 §§7 and 9.

### Approved packages

- M7.1 — `pi-agent-session`: entry tree, lanes, operation records, Send and Local
  storage traits, reducer, recovery, and branching —
  `8c9f3c6c0859757c7c02c0481e4cfe04a803d5bf`
- M7.2 — `pi-agent-env` and `pi-agent-runtime-tokio`: portable capability
  traits, Tokio filesystem and process execution, termination behavior, and
  actor facade — `aa3f90d635f7edac1a36c6084a1b3fcf4e281487`
- M7.0 — deferred responses: versioned serializable `DeferredHandle`, Send and
  Local fetch/cancel execution capabilities, `Models` orchestration, and
  hermetic `ScriptedRuntime` support —
  `9bf40c5bd56fa863e8c5a51dba6779b1dd4a7295`

### Architecture v2 Part 2 §10 conformance now passing

M7 adds 27 previously absent exact §10.10 conformance names. The final M7 tree
contains 306 exact §10 conformance names in total.

§10.10 reducer and session-tree conformance (16 new):

```text
session_sequence_starts_at_one
session_sequence_is_global_across_mutation_kinds
session_sequence_gap_is_corruption
session_entry_parent_must_exist
session_lane_head_moves_on_append
session_lane_can_move_to_ancestor
session_multiple_lanes_share_entry_tree
session_branch_scan_leaf_to_root
session_global_entry_query_sequence_order
session_fact_latest_value_wins
session_label_is_global_not_branch_scoped
session_stats_derive_from_usage_records
session_open_operation_detected
session_multiple_open_operations_is_corruption
session_operation_recovery_reconstructs_intent
session_reducer_replay_equals_live_state
```

§10.10 environment conformance (11 new):

```text
env_read_file
env_write_file
env_atomic_replace
env_process_stdout_stream
env_process_stderr_stream
env_process_exit_status
env_process_graceful_termination
env_process_forced_termination
env_process_tree_termination
env_stdio_grace_period
env_cancellation
```

M7.0 adds eight deferred-response tests covering handle persistence, successful
and failed/cancelled scripted lifecycles, fetch/cancel option propagation,
provider/auth/capability ordering, independent optional capabilities, and the
durable-handle terminal invariant. These tests cite pinned
`packages/ai/test/providers.test.ts`, but deferred responses have no separately
named exact conformance row in Part 2 §10.

### Parity manifest coverage

- Pinned upstream test files mapped: 159/159 — 63 `semantic-parity`, 0
  `deliberate-divergence`, and 96 `planned`.
- Status-bearing mappings: 246 total — 140 `semantic-parity`, 10
  `deliberate-divergence`, and 96 `planned`.
- Future named conformance tests: 1 `planned_test` entry, assigned to M3.4.
- Rust test inventory discovered by the parity checker: 747 unique tests.

### Architecture correction notes

M7 added three corrections, all from M7.1 to Architecture v2 Part 2:

- §7.2 records that Pi session usage records permit negative token and monetary
  adjustments; native response `Usage` remains unsigned, while the operation
  record carries separate cost and signed fixed-point adjustment fields without
  `f64`.
- §7.4 records that replay retains unmatched operation-finished records and
  multiple unresolved starts for corruption diagnosis, while live in-memory and
  JSONL writers reject a second open operation on one lane.
- §7.5 records that Pi branch forks accept only message-entry targets; custom,
  compaction, and other entry targets are rejected as `invalid_fork_target`,
  while whole-tree forks remain unrestricted.

M7.0 and M7.2 added no correction notes to either architecture document.

## 2026-08-26 — M8: harness policies, resources, and orchestration

M8 refreshed the parity baseline to pinned Pi commit
`8fa7eebd235355522c8104166b4f1f959b4e2f10`, completed the harness compaction
and branch-summary policies, added skills, prompt templates, reference tools,
truncation, and telemetry, and established durable harness orchestration over
`pi-agent-core`, `pi-agent-session`, and the environment seams. All provider
and harness tests remain hermetic.

### Approved packages

- M8.0 — pin refresh, pi-ai re-verification, manifest regeneration, OpenAI
  Completions reasoning-detail replay corrections, and refreshed fixtures and
  catalogs — `308c2c59cfcd1b64629929610d82fae11e5c8f4a`
- M8.1 — compaction, branch summarization, `HarnessContextPolicy`, and overflow
  retry — `e58e678b1a40321d1b056173f82f99e895ceab5b`
- M8.2 — skills, prompt templates, reference tools, file mutation queue,
  truncation, and telemetry — `036bcd62f8206cd7b35fc25c172d2a233b89a7d6`
- M8.3 — durable harness orchestration over agent-core, session, and
  environment, including recovery, queue ingress, tool replay policy, events,
  and Send/Local execution — `f1006437cc361ec5fab899825f19df80e4f36f71`

### Architecture v2 Part 2 §10 conformance now passing

M8 adds 38 previously absent exact §10.10 conformance names, bringing the
cumulative exact §10 total to 344.

§10.10 compaction and branch-summary conformance (14 new):

```text
compaction_threshold_decision
compaction_manual_reason
compaction_overflow_reason
compaction_retains_configured_tail
compaction_records_tokens_before
compaction_records_summary_usage
compaction_failure_does_not_move_branch_head
compaction_operation_can_resume
compaction_context_uses_latest_compaction_entry
branch_summary_finds_common_ancestor
branch_summary_summarizes_abandoned_segment
branch_summary_records_from_id
branch_summary_navigation_is_durable
branch_summary_failure_leaves_navigation_recoverable
```

§10.10 environment and reference-tool conformance (9 new):

```text
mutation_queue_same_path_serializes
mutation_queue_different_paths_concurrent
edit_requires_exact_match
edit_rejects_multiple_matches
edit_rejects_noop
truncate_never_splits_utf8
truncate_respects_byte_limit
truncate_respects_line_limit
bash_truncated_output_has_full_artifact
```

§10.10 skills, prompt-template, and telemetry conformance (15 new):

```text
skill_catalog_discovers_valid_skills
skill_invalid_metadata_is_reported
skill_content_digest_is_stable
skill_resume_uses_recorded_digest
prompt_template_argument_substitution
prompt_template_missing_argument_rejected
prompt_template_output_is_deterministic
telemetry_schema_validates_every_event
telemetry_schema_version_is_required
telemetry_default_excludes_content
telemetry_default_excludes_auth
telemetry_default_excludes_replay_payload
telemetry_correlates_session_run_operation
telemetry_durable_event_follows_commit
telemetry_sink_failure_is_best_effort_by_default
```

M8.3 additionally re-exercises the §10.9 Agent gate through durable harness
orchestration, covering lifecycle and event ordering, failed-assistant
commitment, queue persistence and polling boundaries, tool scheduling and
recovery, cancellation, operation recovery, and both Send and Local families.
M8.0 re-verifies pi-ai stream/replay behavior and all existing replay and wire
goldens against the refreshed pin.

### Parity manifest coverage

- Pinned upstream test files mapped: 160/160 — 68 `semantic-parity`, 1
  `deliberate-divergence`, and 91 `planned`.
- Status-bearing mappings: 251 total — 150 `semantic-parity`, 10
  `deliberate-divergence`, and 91 `planned`.
- Future named conformance tests: 1 `planned_test` entry,
  `agent_failed_assistant_is_omitted_from_next_provider_projection`, assigned
  to M3.4.

### Architecture correction notes

M8 added six corrections, all to Architecture v2 Part 2. No M8 package added a
correction to Part 1.

- M8.0 corrected §1.5: OpenAI-compatible reasoning-detail replay merges
  consecutive text and summary deltas, keeps encrypted details discrete, and
  follows Pi's exact metadata assignment and JSON omission behavior.
- M8.1 corrected §7.7: automatic threshold and overflow compaction reuse the
  already-open run operation; only standalone manual compaction starts and
  finishes its own operation.
- M8.1 corrected §7.8: `BranchSummaryEntry::from_id` records the navigation
  operation's source leaf, including summaries appended under the empty root.
- M8.1 corrected §7.8: Send and Local custom-entry projectors run for normal
  durable context reconstruction after latest-compaction transformation, but
  never participate in compaction usage, tail selection, or summary prompts.
- M8.2 corrected §7.9: missing prompt-template positional arguments substitute
  an empty string in Pi-compatible mode; the named strict-policy conformance
  test exercises explicit rejection instead.
- M8.2 corrected §7.11: reference edits support Pi's multi-replacement,
  same-original-file matching, ambiguity and overlap checks, BOM and line-ending
  preservation, and normalized Unicode/whitespace fallback before atomic
  publication.

M8.3 added no correction note to either architecture document.
