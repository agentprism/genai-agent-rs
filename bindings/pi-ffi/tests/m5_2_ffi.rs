use pi_ffi::c_api::{
    PI_STATUS_INVALID_ARGUMENT, pi_agent_cancel, pi_agent_create, pi_agent_destroy, pi_agent_run,
    pi_auth_challenge, pi_auth_challenge_clear, pi_auth_login_begin, pi_auth_session_destroy,
    pi_auth_session_next, pi_last_error_message, pi_models_create, pi_models_destroy,
};
use pi_ffi::{
    PI_EVENT_ENVELOPE_SCHEMA_VERSION, PiAuthResponseStatus, PiAuthSessionEventStatus, PiModels,
};
use serde_json::{Value, json};
use std::ffi::{CStr, CString, c_char, c_void};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

fn models_config(responses: Value) -> String {
    json!({
        "schemaVersion": 1,
        "runtime": {
            "type": "scripted",
            "responses": responses,
        },
    })
    .to_string()
}

fn agent_config() -> String {
    json!({
        "schemaVersion": 1,
        "model": {
            "provider": "scripted",
            "model": "fixture-model",
        },
        "systemPrompt": "You are a fixture agent.",
        "reasoning": "off",
    })
    .to_string()
}

/// Architecture v2 part 2 §10.9 `agent_prompt_text_event_sequence`.
/// Pi basis: packages/agent/src/agent-loop.ts and packages/agent/src/agent.ts.
#[test]
fn agent_prompt_text_event_sequence() {
    let models = PiModels::from_json(&models_config(json!([
        { "type": "text", "text": "hello from ScriptedRuntime" }
    ])))
    .unwrap();
    let agent = models.create_agent(agent_config()).unwrap();
    let run_id = agent.run(json!({ "text": "hello" }).to_string()).unwrap();

    let mut envelopes = Vec::new();
    while let Some(envelope) = agent.next_event(run_id).unwrap() {
        envelopes.push(serde_json::from_str::<Value>(&envelope).unwrap());
    }

    assert_eq!(
        envelopes
            .iter()
            .map(|envelope| envelope["type"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "run_started",
            "turn_started",
            "message_started",
            "message_committed",
            "context_prepared",
            "message_started",
            "assistant_message_started",
            "assistant_content_block_started",
            "assistant_text_delta",
            "assistant_content_block_finished",
            "assistant_finished",
            "message_committed",
            "turn_finished",
            "run_finished",
        ]
    );
    assert_eq!(envelopes[2]["data"]["role"], "user");
    assert_eq!(envelopes[3]["data"]["message"]["role"], "user");
    assert_eq!(envelopes[5]["data"]["role"], "assistant");
    assert_eq!(envelopes[11]["data"]["message"]["role"], "assistant");
    assert_eq!(envelopes[12]["data"]["outcome"]["assistantFinish"], "stop");
    assert_eq!(envelopes[13]["data"]["outcome"]["type"], "completed");
    for (index, envelope) in envelopes.iter().enumerate() {
        assert_eq!(envelope["schemaVersion"], PI_EVENT_ENVELOPE_SCHEMA_VERSION);
        assert_eq!(envelope["sequence"], u64::try_from(index + 1).unwrap());
        assert_eq!(envelope["runId"], run_id.to_string());
    }
    agent.shutdown().unwrap();
}

struct CallbackState {
    envelopes: Mutex<Vec<Value>>,
    terminal: Condvar,
    cancel_issued: Mutex<bool>,
    cancel_gate: Condvar,
}

struct BackToBackCallbackState {
    envelopes: Mutex<Vec<Value>>,
    terminal: Condvar,
}

struct ConcurrentCallbackState {
    envelopes: Mutex<Vec<Value>>,
    delay_run_start: AtomicBool,
}

struct ReentrantCancellationState {
    agent: *mut pi_ffi::c_api::pi_agent_handle,
    run_id: u64,
    envelopes: Mutex<Vec<Value>>,
    terminal: Condvar,
    cancel_issued: AtomicBool,
    cancel_failed: AtomicBool,
}

unsafe extern "C" fn recording_callback(envelope_json: *const c_char, user_data: *mut c_void) {
    let state = unsafe { &*(user_data.cast::<BackToBackCallbackState>()) };
    let Ok(envelope_json) = unsafe { CStr::from_ptr(envelope_json) }.to_str() else {
        return;
    };
    let Ok(envelope) = serde_json::from_str::<Value>(envelope_json) else {
        return;
    };
    let terminal = envelope["type"] == "run_finished";
    lock_unpoisoned(&state.envelopes).push(envelope);
    if terminal {
        state.terminal.notify_all();
    }
}

/// Architecture v2 part 1 §6 lossless JSON envelope. This is the C-ABI
/// regression for the round-four probe: tool-owned argument keys are data and
/// must not be rewritten as binding schema names.
#[test]
fn ffi_c_abi_event_envelope_preserves_tool_arguments() {
    let arguments = json!({
        "snake_case_arg": {
            "inner_key": "owner-value"
        }
    });
    let models_json = CString::new(models_config(json!([
        {
            "type": "tool_call",
            "name": "missing_tool",
            "arguments": arguments.clone(),
        },
        { "type": "text", "text": "done" },
    ])))
    .unwrap();
    let agent_json = CString::new(agent_config()).unwrap();
    let input_json = CString::new(json!({ "text": "use the tool" }).to_string()).unwrap();
    let models = unsafe { pi_models_create(models_json.as_ptr()) };
    assert!(!models.is_null());
    let agent = unsafe { pi_agent_create(models, agent_json.as_ptr()) };
    assert!(!agent.is_null());

    let state = Box::new(BackToBackCallbackState {
        envelopes: Mutex::new(Vec::new()),
        terminal: Condvar::new(),
    });
    let state_pointer = Box::into_raw(state);
    let run_id = unsafe {
        pi_agent_run(
            agent,
            input_json.as_ptr(),
            Some(recording_callback),
            state_pointer.cast(),
        )
    };
    assert_ne!(run_id, 0);

    let state = unsafe { &*state_pointer };
    let guard = lock_unpoisoned(&state.envelopes);
    let (guard, timeout) = state
        .terminal
        .wait_timeout_while(guard, Duration::from_secs(5), |envelopes| {
            !envelopes
                .iter()
                .any(|envelope| envelope["type"] == "run_finished")
        })
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(!timeout.timed_out(), "C-ABI probe did not finish");
    let tool_started = guard
        .iter()
        .find(|envelope| envelope["type"] == "tool_execution_started")
        .expect("tool execution start envelope");
    assert_eq!(tool_started["data"]["call"]["arguments"], arguments);
    assert!(
        tool_started["data"]["call"]["arguments"]
            .get("snakeCaseArg")
            .is_none()
    );
    drop(guard);

    unsafe {
        pi_agent_destroy(agent);
        pi_models_destroy(models);
        drop(Box::from_raw(state_pointer));
    }
}

unsafe extern "C" fn concurrent_recording_callback(
    envelope_json: *const c_char,
    user_data: *mut c_void,
) {
    let state = unsafe { &*(user_data.cast::<ConcurrentCallbackState>()) };
    let Ok(envelope_json) = unsafe { CStr::from_ptr(envelope_json) }.to_str() else {
        return;
    };
    let Ok(envelope) = serde_json::from_str::<Value>(envelope_json) else {
        return;
    };
    let run_started = envelope["type"] == "run_started";
    lock_unpoisoned(&state.envelopes).push(envelope);
    if run_started && state.delay_run_start.swap(false, Ordering::AcqRel) {
        // Keep the accepted actor command active long enough for every
        // concurrent C caller to enqueue its own atomic prompt-and-sink
        // command. Those commands must be rejected without invoking a sink.
        thread::sleep(Duration::from_millis(200));
    }
}

/// Architecture v2 part 2 §10.9 `agent_handle_event_sinks_are_barriers`.
/// Pi basis: packages/agent/src/agent.ts:221-238,493-592. Part 1 §6 and part
/// 2 §9.4 strengthen the C adapter by binding each sink to one accepted actor
/// command rather than registering it globally before prompt acceptance.
#[test]
fn agent_handle_event_sinks_are_barriers() {
    const CALLS: usize = 20;

    let responses = (0..CALLS)
        .map(|index| json!({ "type": "text", "text": format!("response-{index}") }))
        .collect::<Vec<_>>();
    let models_json = CString::new(models_config(json!(responses))).unwrap();
    let agent_json = CString::new(agent_config()).unwrap();
    let models = unsafe { pi_models_create(models_json.as_ptr()) };
    assert!(!models.is_null());
    let agent = unsafe { pi_agent_create(models, agent_json.as_ptr()) };
    assert!(!agent.is_null());

    let states = (0..CALLS)
        .map(|_| {
            Box::new(ConcurrentCallbackState {
                envelopes: Mutex::new(Vec::new()),
                delay_run_start: AtomicBool::new(true),
            })
        })
        .collect::<Vec<_>>();
    let start = Arc::new(Barrier::new(CALLS));
    let agent_address = agent as usize;
    let workers = states
        .iter()
        .enumerate()
        .map(|(index, state)| {
            let start = Arc::clone(&start);
            let state_address = (&**state as *const ConcurrentCallbackState) as usize;
            thread::spawn(move || {
                let input_json =
                    CString::new(json!({ "text": format!("concurrent-{index}") }).to_string())
                        .unwrap();
                start.wait();
                let run_id = unsafe {
                    pi_agent_run(
                        agent_address as *mut _,
                        input_json.as_ptr(),
                        Some(concurrent_recording_callback),
                        state_address as *mut c_void,
                    )
                };
                (index, run_id)
            })
        })
        .collect::<Vec<_>>();
    let results = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect::<Vec<_>>();

    unsafe {
        pi_agent_destroy(agent);
    }

    let mut accepted = 0;
    let mut rejected = 0;
    for (index, run_id) in results {
        let envelopes = lock_unpoisoned(&states[index].envelopes);
        if run_id == 0 {
            rejected += 1;
            assert!(
                envelopes.is_empty(),
                "rejected C call {index} received {} envelopes",
                envelopes.len()
            );
            continue;
        }

        accepted += 1;
        let expected_run_id = run_id.to_string();
        assert_eq!(envelopes.first().unwrap()["type"], "run_started");
        assert_eq!(envelopes.last().unwrap()["type"], "run_finished");
        assert!(
            envelopes
                .iter()
                .all(|envelope| { envelope["runId"].as_str() == Some(expected_run_id.as_str()) })
        );
        assert_eq!(
            envelopes
                .iter()
                .filter(|envelope| envelope["type"] == "run_started")
                .count(),
            1
        );
        assert_eq!(
            envelopes
                .iter()
                .filter(|envelope| envelope["type"] == "run_finished")
                .count(),
            1
        );
    }
    assert!(accepted > 0, "the concurrent probe accepted no run");
    assert!(
        rejected > 0,
        "the concurrent probe did not exercise rejection"
    );

    unsafe {
        pi_models_destroy(models);
    }
}

/// Architecture v2 part 2 §10.9 `agent_run_finished_is_final_event`.
/// Pi basis: packages/agent/src/agent-loop.ts and packages/agent/src/agent.ts.
#[test]
fn agent_run_finished_is_final_event() {
    const RUNS: usize = 100;

    let responses = (0..RUNS)
        .map(|index| json!({ "type": "text", "text": format!("response-{index}") }))
        .collect::<Vec<_>>();
    let models_json = CString::new(models_config(json!(responses))).unwrap();
    let agent_json = CString::new(agent_config()).unwrap();
    let input_json = CString::new(json!({ "text": "next" }).to_string()).unwrap();
    let models = unsafe { pi_models_create(models_json.as_ptr()) };
    assert!(!models.is_null());
    let agent = unsafe { pi_agent_create(models, agent_json.as_ptr()) };
    assert!(!agent.is_null());

    let state = Box::new(BackToBackCallbackState {
        envelopes: Mutex::new(Vec::new()),
        terminal: Condvar::new(),
    });
    let state_pointer = Box::into_raw(state);
    let mut run_ids = Vec::with_capacity(RUNS);

    for _ in 0..RUNS {
        let run_id = unsafe {
            pi_agent_run(
                agent,
                input_json.as_ptr(),
                Some(recording_callback),
                state_pointer.cast(),
            )
        };
        assert_ne!(run_id, 0, "back-to-back C run failed to start");
        run_ids.push(run_id);

        let state = unsafe { &*state_pointer };
        let run_id_string = run_id.to_string();
        let guard = lock_unpoisoned(&state.envelopes);
        let (_guard, timeout) = state
            .terminal
            .wait_timeout_while(guard, Duration::from_secs(5), |envelopes| {
                !envelopes.iter().any(|envelope| {
                    envelope["runId"].as_str() == Some(&run_id_string)
                        && envelope["type"] == "run_finished"
                })
            })
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(!timeout.timed_out(), "run {run_id} did not finish");
    }

    unsafe {
        pi_agent_destroy(agent);
    }

    let state = unsafe { &*state_pointer };
    let envelopes = lock_unpoisoned(&state.envelopes);
    for run_id in run_ids {
        let run_id_string = run_id.to_string();
        let run_envelopes = envelopes
            .iter()
            .filter(|envelope| envelope["runId"].as_str() == Some(&run_id_string))
            .collect::<Vec<_>>();
        assert_eq!(run_envelopes.first().unwrap()["type"], "run_started");
        assert_eq!(run_envelopes.last().unwrap()["type"], "run_finished");
        assert_eq!(
            run_envelopes
                .iter()
                .filter(|envelope| envelope["type"] == "run_finished")
                .count(),
            1,
            "run {run_id} received events from a later core run"
        );
    }
    assert!(
        envelopes
            .windows(2)
            .all(|pair| pair[0]["sequence"].as_u64() < pair[1]["sequence"].as_u64())
    );
    drop(envelopes);

    unsafe {
        pi_models_destroy(models);
        drop(Box::from_raw(state_pointer));
    }
}

unsafe extern "C" fn cancelling_callback(envelope_json: *const c_char, user_data: *mut c_void) {
    let state = unsafe { &*(user_data.cast::<CallbackState>()) };
    let Ok(envelope_json) = unsafe { CStr::from_ptr(envelope_json) }.to_str() else {
        return;
    };
    let Ok(envelope) = serde_json::from_str::<Value>(envelope_json) else {
        return;
    };
    let event_type = envelope["type"].as_str().unwrap_or_default().to_owned();
    if event_type == "run_started" {
        let mut cancel_issued = lock_unpoisoned(&state.cancel_issued);
        while !*cancel_issued {
            cancel_issued = state
                .cancel_gate
                .wait(cancel_issued)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }
    let mut envelopes = lock_unpoisoned(&state.envelopes);
    envelopes.push(envelope);
    if event_type == "run_finished" {
        state.terminal.notify_all();
    }
}

unsafe extern "C" fn reentrant_cancelling_callback(
    envelope_json: *const c_char,
    user_data: *mut c_void,
) {
    let state = unsafe { &*(user_data.cast::<ReentrantCancellationState>()) };
    let Ok(envelope_json) = unsafe { CStr::from_ptr(envelope_json) }.to_str() else {
        return;
    };
    let Ok(envelope) = serde_json::from_str::<Value>(envelope_json) else {
        return;
    };
    let event_type = envelope["type"].as_str().unwrap_or_default().to_owned();
    if event_type == "run_started" && !state.cancel_issued.swap(true, Ordering::AcqRel) {
        unsafe {
            pi_agent_cancel(state.agent, state.run_id);
        }
        state
            .cancel_failed
            .store(!pi_last_error_message().is_null(), Ordering::Release);
    }
    lock_unpoisoned(&state.envelopes).push(envelope);
    if event_type == "run_finished" {
        state.terminal.notify_all();
    }
}

/// Architecture v2 part 2 §10.9 `agent_cancelled_assistant_is_committed`.
/// Pi basis: packages/agent/src/agent-loop.ts:281-352 and agent.ts:493-592.
#[test]
fn agent_cancelled_assistant_is_committed() {
    let models_json = CString::new(models_config(json!([
        { "type": "text", "text": "this response is cancelled at run start" }
    ])))
    .unwrap();
    let agent_json = CString::new(agent_config()).unwrap();
    let input_json = CString::new(json!({ "text": "cancel" }).to_string()).unwrap();
    let models = unsafe { pi_models_create(models_json.as_ptr()) };
    assert!(!models.is_null());
    let agent = unsafe { pi_agent_create(models, agent_json.as_ptr()) };
    assert!(!agent.is_null());

    let state = Box::new(CallbackState {
        envelopes: Mutex::new(Vec::new()),
        terminal: Condvar::new(),
        cancel_issued: Mutex::new(false),
        cancel_gate: Condvar::new(),
    });
    let state_pointer = Box::into_raw(state);
    let run_id = unsafe {
        pi_agent_run(
            agent,
            input_json.as_ptr(),
            Some(cancelling_callback),
            state_pointer.cast(),
        )
    };
    assert_ne!(run_id, 0);

    let state = unsafe { &*state_pointer };
    unsafe {
        pi_agent_cancel(agent, run_id);
    }
    assert!(
        pi_last_error_message().is_null(),
        "immediate cancellation rejected the binding run identity"
    );
    *lock_unpoisoned(&state.cancel_issued) = true;
    state.cancel_gate.notify_all();

    let guard = lock_unpoisoned(&state.envelopes);
    let (guard, timeout) = state
        .terminal
        .wait_timeout_while(guard, Duration::from_secs(5), |envelopes| {
            !envelopes
                .iter()
                .any(|envelope| envelope["type"] == "run_finished")
        })
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(!timeout.timed_out(), "cancelled run did not finish");
    assert_eq!(guard.last().unwrap()["type"], "run_finished");
    assert_eq!(
        guard.last().unwrap()["data"]["outcome"]["type"],
        "cancelled"
    );
    assert!(guard.iter().any(|envelope| {
        envelope["type"] == "message_committed"
            && envelope["data"]["message"]["role"] == "assistant"
            && envelope["data"]["message"]["finish"]["reason"] == "aborted"
    }));
    drop(guard);

    unsafe {
        pi_agent_destroy(agent);
        pi_models_destroy(models);
        drop(Box::from_raw(state_pointer));
    }
}

/// Architecture v2 part 2 §9.4 callback regression: synchronous foreign
/// cancellation must not wait behind the actor sink currently invoking it.
/// Pi basis: packages/agent/src/agent.ts:493-592 permits abort during active
/// event delivery; the C API forbids only destruction from an event callback.
#[test]
fn ffi_callback_initiated_cancellation_is_nonblocking() {
    let models_json = CString::new(models_config(json!([
        { "type": "text", "text": "callback cancellation must win" }
    ])))
    .unwrap();
    let agent_json = CString::new(agent_config()).unwrap();
    let input_json = CString::new(json!({ "text": "cancel reentrantly" }).to_string()).unwrap();
    let models = unsafe { pi_models_create(models_json.as_ptr()) };
    assert!(!models.is_null());
    let agent = unsafe { pi_agent_create(models, agent_json.as_ptr()) };
    assert!(!agent.is_null());

    // A fresh binding agent allocates external run identities from one. The
    // callback can run before `pi_agent_run` returns, so the first identity is
    // provisioned in the callback state up front.
    let state = Box::new(ReentrantCancellationState {
        agent,
        run_id: 1,
        envelopes: Mutex::new(Vec::new()),
        terminal: Condvar::new(),
        cancel_issued: AtomicBool::new(false),
        cancel_failed: AtomicBool::new(false),
    });
    let state_pointer = Box::into_raw(state);
    let run_id = unsafe {
        pi_agent_run(
            agent,
            input_json.as_ptr(),
            Some(reentrant_cancelling_callback),
            state_pointer.cast(),
        )
    };
    assert_eq!(run_id, 1);

    let state = unsafe { &*state_pointer };
    let guard = lock_unpoisoned(&state.envelopes);
    let (guard, timeout) = state
        .terminal
        .wait_timeout_while(guard, Duration::from_secs(5), |envelopes| {
            !envelopes
                .iter()
                .any(|envelope| envelope["type"] == "run_finished")
        })
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(
        !timeout.timed_out(),
        "callback-initiated cancellation blocked actor progress"
    );
    assert!(state.cancel_issued.load(Ordering::Acquire));
    assert!(
        !state.cancel_failed.load(Ordering::Acquire),
        "callback-initiated cancellation returned a binding error"
    );
    assert_eq!(
        guard.last().unwrap()["data"]["outcome"]["type"],
        "cancelled"
    );
    assert!(guard.iter().any(|envelope| {
        envelope["type"] == "message_committed"
            && envelope["data"]["message"]["role"] == "assistant"
            && envelope["data"]["message"]["finish"]["reason"] == "aborted"
    }));
    drop(guard);

    unsafe {
        pi_agent_destroy(agent);
        pi_models_destroy(models);
        drop(Box::from_raw(state_pointer));
    }
}

fn auth_models_config() -> String {
    json!({
        "schemaVersion": 1,
        "runtime": {
            "type": "scripted",
            "responses": [],
        },
        "authProviders": [{
            "providerId": "scripted-device",
            "deviceCode": {
                "challengeId": "device-challenge",
                "userCode": "ABCD-EFGH",
                "verificationUri": "https://example.test/device",
                "intervalSeconds": 5,
                "expiresInSeconds": 900,
                "pendingPolls": 0,
                "accessToken": "access-secret",
                "refreshToken": "refresh-secret",
                "expiresAt": 4102444800000_i64,
            },
        }],
    })
    .to_string()
}

fn callback_manual_auth_models_config() -> String {
    json!({
        "schemaVersion": 1,
        "runtime": {
            "type": "scripted",
            "responses": [],
        },
        "authProviders": [{
            "providerId": "scripted-callback-manual",
            "callbackManual": {
                "challengeId": "shared-challenge",
                "authorizationUrl": "https://example.test/authorize?state=expected-state",
                "redirectScheme": "fixture-app",
                "redirectPath": "/oauth/callback",
                "expectedState": "expected-state",
                "authorizationCode": "authorization-code",
                "accessToken": "callback-access-secret",
                "refreshToken": "callback-refresh-secret",
                "expiresAt": 4102444800000_i64,
            },
        }],
    })
    .to_string()
}

fn begin_callback_manual_session(
    models: &PiModels,
    manual_paste: bool,
) -> std::sync::Arc<pi_ffi::PiAuthSession> {
    models
        .auth_login_begin(
            "scripted-callback-manual".into(),
            "oauth".into(),
            json!({
                "externalBrowser": true,
                "customUrlScheme": true,
                "manualPaste": manual_paste,
            })
            .to_string(),
        )
        .unwrap()
}

fn read_shared_challenge(session: &pi_ffi::PiAuthSession, also_accepts_manual_code: bool) {
    let event = session.next().unwrap();
    assert_eq!(event.status, PiAuthSessionEventStatus::Challenge);
    assert_eq!(
        serde_json::from_str::<Value>(&event.json).unwrap(),
        json!({
            "schemaVersion": 1,
            "id": "shared-challenge",
            "type": "open_url",
            "url": "https://example.test/authorize?state=expected-state",
            "redirect": {
                "strategy": "custom_scheme",
                "uri": "fixture-app://callback/oauth/callback",
            },
            "instructions": "Complete login in the browser or provide the authorization code manually",
            "alsoAcceptsManualCode": also_accepts_manual_code,
        })
    );
}

fn assert_auth_session_completes(session: &pi_ffi::PiAuthSession) {
    let progress = session.next().unwrap();
    assert_eq!(progress.status, PiAuthSessionEventStatus::Challenge);
    assert_eq!(
        serde_json::from_str::<Value>(&progress.json).unwrap()["type"],
        "progress"
    );
    let completed = session.next().unwrap();
    assert_eq!(completed.status, PiAuthSessionEventStatus::Completed);
    assert_eq!(
        serde_json::from_str::<Value>(&completed.json).unwrap()["type"],
        "completed"
    );
}

/// Architecture v2 part 2 §6.5: device polling remains in Rust and the host
/// receives only the device challenge, progress, and completion state.
#[test]
fn ffi_auth_session_completes_scripted_device_code_flow() {
    let models = PiModels::from_json(&auth_models_config()).unwrap();
    let session = models
        .auth_login_begin(
            "scripted-device".into(),
            "oauth".into(),
            json!({
                "externalBrowser": true,
                "manualPaste": true,
            })
            .to_string(),
        )
        .unwrap();

    let first = session.next().unwrap();
    assert_eq!(first.status, PiAuthSessionEventStatus::Challenge);
    let first_json: Value = serde_json::from_str(&first.json).unwrap();
    assert_eq!(first_json["type"], "device_code");
    assert_eq!(first_json["userCode"], "ABCD-EFGH");

    let progress = session.next().unwrap();
    assert_eq!(progress.status, PiAuthSessionEventStatus::Challenge);
    assert_eq!(
        serde_json::from_str::<Value>(&progress.json).unwrap()["type"],
        "progress"
    );

    let completed = session.next().unwrap();
    assert_eq!(completed.status, PiAuthSessionEventStatus::Completed);
    assert_eq!(
        serde_json::from_str::<Value>(&completed.json).unwrap()["type"],
        "completed"
    );
}

/// Architecture v2 part 2 §6.5 exact `open_url` challenge schema. The resolved
/// redirect is part of the one public challenge, and manual-code acceptance is
/// exposed and enforced only when the host advertises that capability.
#[test]
fn ffi_auth_open_url_challenge_exact_schema_and_capability() {
    let models = PiModels::from_json(&callback_manual_auth_models_config()).unwrap();

    let manual_session = begin_callback_manual_session(&models, true);
    read_shared_challenge(&manual_session, true);
    assert_eq!(
        manual_session
            .respond(
                "shared-challenge".into(),
                json!({ "type": "manual_code", "value": "authorization-code" }).to_string(),
            )
            .unwrap(),
        PiAuthResponseStatus::Accepted
    );
    assert_auth_session_completes(&manual_session);

    let callback_only_session = begin_callback_manual_session(&models, false);
    read_shared_challenge(&callback_only_session, false);
    assert!(
        callback_only_session
            .respond(
                "shared-challenge".into(),
                json!({ "type": "manual_code", "value": "authorization-code" }).to_string(),
            )
            .is_err(),
        "manual code was accepted without the advertised host capability"
    );
    assert_eq!(
        callback_only_session
            .respond(
                "shared-challenge".into(),
                json!({
                    "type": "redirect_arrived",
                    "url": "fixture-app://callback/oauth/callback?code=authorization-code&state=expected-state"
                })
                .to_string(),
            )
            .unwrap(),
        PiAuthResponseStatus::Accepted
    );
    assert_auth_session_completes(&callback_only_session);
}

/// Architecture v2 part 1 §6 and part 2 §6.5 C surface: null output/session
/// pointers and a non-empty output slot are invalid arguments, matching the
/// exported header and `pi_auth_session_respond` classification.
#[test]
fn ffi_auth_session_next_invalid_arguments_return_invalid_argument() {
    let models_json = CString::new(callback_manual_auth_models_config()).unwrap();
    let provider_id = CString::new("scripted-callback-manual").unwrap();
    let auth_type = CString::new("oauth").unwrap();
    let capabilities = CString::new(
        json!({
            "externalBrowser": true,
            "customUrlScheme": true,
            "manualPaste": true,
        })
        .to_string(),
    )
    .unwrap();
    let models = unsafe { pi_models_create(models_json.as_ptr()) };
    assert!(!models.is_null());
    let session = unsafe {
        pi_auth_login_begin(
            models,
            provider_id.as_ptr(),
            auth_type.as_ptr(),
            capabilities.as_ptr(),
        )
    };
    assert!(!session.is_null());

    let mut output = pi_auth_challenge {
        json: ptr::null_mut(),
    };
    assert_eq!(
        unsafe { pi_auth_session_next(ptr::null_mut(), &mut output) },
        PI_STATUS_INVALID_ARGUMENT
    );
    assert_eq!(
        unsafe { pi_auth_session_next(session, ptr::null_mut()) },
        PI_STATUS_INVALID_ARGUMENT
    );

    output.json = std::ptr::NonNull::<c_char>::dangling().as_ptr();
    assert_eq!(
        unsafe { pi_auth_session_next(session, &mut output) },
        PI_STATUS_INVALID_ARGUMENT
    );
    output.json = ptr::null_mut();

    unsafe {
        pi_auth_challenge_clear(&mut output);
        pi_auth_session_destroy(session);
        pi_models_destroy(models);
    }
}

/// Architecture v2 part 2 §10.7 `auth_late_losing_response_is_superseded`.
/// Pi basis: packages/ai/src/auth/oauth/openai-codex.ts:433-499; the explicit
/// superseded result is the §10.11 host-owned redirect divergence replacement.
#[test]
fn auth_late_losing_response_is_superseded() {
    let models = PiModels::from_json(&callback_manual_auth_models_config()).unwrap();
    let session = begin_callback_manual_session(&models, true);
    read_shared_challenge(&session, true);

    let accepted = session
        .respond(
            "shared-challenge".into(),
            json!({ "type": "manual_code", "value": "authorization-code" }).to_string(),
        )
        .unwrap();
    assert_eq!(accepted, PiAuthResponseStatus::Accepted);

    assert_auth_session_completes(&session);

    let late_callback = session
        .respond(
            "shared-challenge".into(),
            json!({
                "type": "redirect_arrived",
                "url": "fixture-app://callback/oauth/callback?code=authorization-code&state=expected-state"
            })
            .to_string(),
        )
        .unwrap();
    assert_eq!(late_callback, PiAuthResponseStatus::ChallengeSuperseded);
}

/// Architecture v2 part 2 §10.7 `auth_callback_and_manual_first_valid_wins`.
/// Pi basis: packages/ai/src/auth/oauth/openai-codex.ts:433-499. The callback
/// and manual waiters share one public FFI challenge identity.
#[test]
fn auth_callback_and_manual_first_valid_wins() {
    let models = PiModels::from_json(&callback_manual_auth_models_config()).unwrap();

    {
        let session = begin_callback_manual_session(&models, true);
        read_shared_challenge(&session, true);

        let accepted = session
            .respond(
                "shared-challenge".into(),
                json!({
                    "type": "redirect_arrived",
                    "url": "fixture-app://callback/oauth/callback?code=authorization-code&state=expected-state"
                })
                .to_string(),
            )
            .unwrap();
        assert_eq!(accepted, PiAuthResponseStatus::Accepted);

        assert_auth_session_completes(&session);
        let late_manual = session
            .respond(
                "shared-challenge".into(),
                json!({ "type": "manual_code", "value": "authorization-code" }).to_string(),
            )
            .unwrap();
        assert_eq!(late_manual, PiAuthResponseStatus::ChallengeSuperseded);
    }

    {
        let session = begin_callback_manual_session(&models, true);
        read_shared_challenge(&session, true);

        let invalid_manual = session
            .respond(
                "shared-challenge".into(),
                json!({ "type": "manual_code", "value": "invalid-code" }).to_string(),
            )
            .unwrap();
        assert_eq!(invalid_manual, PiAuthResponseStatus::Accepted);

        let valid_callback = session
            .respond(
                "shared-challenge".into(),
                json!({
                    "type": "redirect_arrived",
                    "url": "fixture-app://callback/oauth/callback?code=authorization-code&state=expected-state"
                })
                .to_string(),
            )
            .unwrap();
        assert_eq!(valid_callback, PiAuthResponseStatus::Accepted);
        assert_auth_session_completes(&session);
    }
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
