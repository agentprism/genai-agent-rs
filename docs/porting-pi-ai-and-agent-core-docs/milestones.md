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
