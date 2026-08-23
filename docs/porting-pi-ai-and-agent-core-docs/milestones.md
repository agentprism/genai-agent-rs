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
