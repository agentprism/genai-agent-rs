use pi_ffi::c_api::{
    PI_AUTH_CHALLENGE_SUPERSEDED, PI_STATUS_COMPLETE, PI_STATUS_ERROR, PI_STATUS_OK,
    pi_agent_cancel, pi_agent_create, pi_agent_destroy, pi_agent_handle, pi_agent_run,
    pi_auth_challenge, pi_auth_challenge_clear, pi_auth_login_begin, pi_auth_session_destroy,
    pi_auth_session_next, pi_auth_session_respond, pi_last_error_message, pi_models_create,
    pi_models_destroy,
};
use serde_json::{Value, json};
use std::error::Error;
use std::ffi::{CStr, CString, c_char, c_void};
use std::ptr;
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::Duration;

struct HostState {
    agent: usize,
    envelopes: Mutex<Vec<Value>>,
    terminal: Condvar,
}

unsafe extern "C" fn on_agent_event(envelope_json: *const c_char, user_data: *mut c_void) {
    let state = unsafe { &*(user_data.cast::<HostState>()) };
    let Ok(json) = unsafe { CStr::from_ptr(envelope_json) }.to_str() else {
        return;
    };
    let Ok(envelope) = serde_json::from_str::<Value>(json) else {
        return;
    };
    let event_type = envelope["type"].as_str().unwrap_or_default().to_owned();
    if event_type == "run_started"
        && let Some(run_id) = envelope["runId"]
            .as_str()
            .and_then(|run_id| run_id.parse::<u64>().ok())
    {
        unsafe {
            pi_agent_cancel(state.agent as *mut pi_agent_handle, run_id);
        }
    }
    println!("{json}");
    let mut envelopes = lock_unpoisoned(&state.envelopes);
    envelopes.push(envelope);
    if event_type == "run_finished" {
        state.terminal.notify_all();
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let models_config = CString::new(
        json!({
            "schemaVersion": 1,
            "runtime": {
                "type": "scripted",
                "responses": [{
                    "type": "text",
                    "text": "This response is cancelled by the example host.",
                }],
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
                    "accessToken": "example-access-secret",
                    "refreshToken": "example-refresh-secret",
                    "expiresAt": 4102444800000_i64,
                },
            }],
        })
        .to_string(),
    )?;
    let agent_config = CString::new(
        json!({
            "schemaVersion": 1,
            "model": {
                "provider": "scripted",
                "model": "fixture-model",
            },
            "systemPrompt": "You are a hermetic binding fixture.",
            "reasoning": "off",
        })
        .to_string(),
    )?;
    let input = CString::new(json!({ "text": "cancel this run" }).to_string())?;

    let models = unsafe { pi_models_create(models_config.as_ptr()) };
    require_handle(!models.is_null(), "pi_models_create")?;
    let agent = unsafe { pi_agent_create(models, agent_config.as_ptr()) };
    require_handle(!agent.is_null(), "pi_agent_create")?;

    let host = Box::new(HostState {
        agent: agent as usize,
        envelopes: Mutex::new(Vec::new()),
        terminal: Condvar::new(),
    });
    let host_pointer = Box::into_raw(host);
    let run_id = unsafe {
        pi_agent_run(
            agent,
            input.as_ptr(),
            Some(on_agent_event),
            host_pointer.cast(),
        )
    };
    require_handle(run_id != 0, "pi_agent_run")?;

    let host = unsafe { &*host_pointer };
    let envelopes = lock_unpoisoned(&host.envelopes);
    let (envelopes, timeout) = host
        .terminal
        .wait_timeout_while(envelopes, Duration::from_secs(5), |envelopes| {
            !envelopes
                .iter()
                .any(|envelope| envelope["type"] == "run_finished")
        })
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if timeout.timed_out() {
        return Err("timed out waiting for cancelled run".into());
    }
    for (index, envelope) in envelopes.iter().enumerate() {
        if envelope["sequence"] != u64::try_from(index + 1)? {
            return Err("event envelope sequence was not monotonic".into());
        }
    }
    if envelopes.last().and_then(|event| event["type"].as_str()) != Some("run_finished") {
        return Err("RunFinished was not the final event".into());
    }
    drop(envelopes);

    let provider = CString::new("scripted-device")?;
    let auth_type = CString::new("oauth")?;
    let capabilities = CString::new(
        json!({
            "externalBrowser": true,
            "manualPaste": true,
        })
        .to_string(),
    )?;
    let session = unsafe {
        pi_auth_login_begin(
            models,
            provider.as_ptr(),
            auth_type.as_ptr(),
            capabilities.as_ptr(),
        )
    };
    require_handle(!session.is_null(), "pi_auth_login_begin")?;

    loop {
        let mut challenge = pi_auth_challenge {
            json: ptr::null_mut(),
        };
        let status = unsafe { pi_auth_session_next(session, &mut challenge) };
        if challenge.json.is_null() {
            return Err(format!("auth session returned no JSON: {}", last_error()).into());
        }
        let challenge_json = unsafe { CStr::from_ptr(challenge.json) }
            .to_str()?
            .to_owned();
        println!("auth: {challenge_json}");
        unsafe {
            pi_auth_challenge_clear(&mut challenge);
        }
        match status {
            PI_STATUS_OK => {}
            PI_STATUS_COMPLETE => break,
            PI_STATUS_ERROR => return Err("scripted device login failed".into()),
            other => return Err(format!("unexpected auth status {other}").into()),
        }
    }

    let challenge_id = CString::new("device-challenge")?;
    let late_response = CString::new(
        json!({
            "type": "manual_code",
            "value": "too-late",
        })
        .to_string(),
    )?;
    let late_status =
        unsafe { pi_auth_session_respond(session, challenge_id.as_ptr(), late_response.as_ptr()) };
    if late_status != PI_AUTH_CHALLENGE_SUPERSEDED {
        return Err("late auth response was not superseded".into());
    }

    unsafe {
        pi_auth_session_destroy(session);
        pi_agent_destroy(agent);
        pi_models_destroy(models);
        drop(Box::from_raw(host_pointer));
    }
    Ok(())
}

fn require_handle(condition: bool, operation: &str) -> Result<(), Box<dyn Error>> {
    if condition {
        Ok(())
    } else {
        Err(format!("{operation} failed: {}", last_error()).into())
    }
}

fn last_error() -> String {
    let error = pi_last_error_message();
    if error.is_null() {
        "unknown binding error".into()
    } else {
        unsafe { CStr::from_ptr(error) }
            .to_string_lossy()
            .into_owned()
    }
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
