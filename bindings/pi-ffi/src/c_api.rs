//! Stable C ABI over opaque handles and owned JSON strings.

#![allow(non_camel_case_types)]

use crate::{
    PiAgent, PiAuthResponseStatus, PiAuthSession, PiAuthSessionEventStatus, PiModels,
    RawEventCallback,
};
use std::cell::RefCell;
use std::ffi::{CStr, CString, c_char, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::sync::Arc;

/// Operation completed or a challenge was returned.
pub const PI_STATUS_OK: i32 = 0;
/// An auth session completed and persisted its credential.
pub const PI_STATUS_COMPLETE: i32 = 1;
/// An operation was cancelled.
pub const PI_STATUS_CANCELLED: i32 = 2;
/// A late auth response lost a challenge race.
pub const PI_AUTH_CHALLENGE_SUPERSEDED: i32 = 3;
/// A general operation failure. Inspect [`pi_last_error_message`].
pub const PI_STATUS_ERROR: i32 = -1;
/// A null pointer, invalid UTF-8, invalid JSON, or invalid challenge response.
pub const PI_STATUS_INVALID_ARGUMENT: i32 = -2;

/// Callback invoked synchronously and in sequence by a binding run worker.
/// The JSON pointer is valid only for the duration of the callback.
pub type pi_event_callback =
    Option<unsafe extern "C" fn(envelope_json: *const c_char, user_data: *mut c_void)>;

/// Opaque model/control-plane handle.
#[repr(C)]
pub struct pi_models_handle {
    models: Arc<PiModels>,
}

/// Opaque serialized-agent handle.
#[repr(C)]
pub struct pi_agent_handle {
    agent: Arc<PiAgent>,
}

/// Opaque host-driven auth-session handle.
#[repr(C)]
pub struct pi_auth_session {
    session: Arc<PiAuthSession>,
}

/// C-owned view of one auth challenge or terminal payload.
///
/// Initialize `json` to null before calling [`pi_auth_session_next`], then
/// release it with [`pi_auth_challenge_clear`].
#[repr(C)]
pub struct pi_auth_challenge {
    /// Rust-allocated NUL-terminated versioned JSON.
    pub json: *mut c_char,
}

thread_local! {
    static LAST_ERROR: RefCell<Option<CString>> = const { RefCell::new(None) };
}

/// Returns the current thread's last sanitized binding error.
///
/// The returned pointer remains valid until the next C binding call on this
/// thread. It must not be freed.
#[unsafe(no_mangle)]
pub extern "C" fn pi_last_error_message() -> *const c_char {
    LAST_ERROR.with(|slot| {
        slot.borrow()
            .as_ref()
            .map_or(ptr::null(), |message| message.as_ptr())
    })
}

/// Creates the model/runtime control-plane handle from versioned JSON.
/// Returns null on failure.
///
/// # Safety
///
/// `config_json` must point to a readable NUL-terminated string for the
/// duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pi_models_create(config_json: *const c_char) -> *mut pi_models_handle {
    clear_last_error();
    match ffi_result(|| {
        let config = unsafe { required_string(config_json, "config_json") }?;
        PiModels::from_json(&config)
            .map(|models| Box::into_raw(Box::new(pi_models_handle { models })))
            .map_err(|error| error.to_string())
    }) {
        Some(handle) => handle,
        None => ptr::null_mut(),
    }
}

/// Releases one model handle. Agents and auth sessions already created from it
/// retain the internal capabilities they need.
///
/// # Safety
///
/// `handle` must be null or a live handle returned by [`pi_models_create`],
/// and it must be destroyed at most once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pi_models_destroy(handle: *mut pi_models_handle) {
    clear_last_error();
    if handle.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| unsafe {
        drop(Box::from_raw(handle));
    }))
    .map_err(|_| set_last_error("panic while destroying models handle"));
}

/// Creates a serialized agent around the model handle's narrow runtime.
/// Returns null on failure.
///
/// # Safety
///
/// `models` must be a live model handle and `agent_config_json` must point to a
/// readable NUL-terminated string for the duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pi_agent_create(
    models: *mut pi_models_handle,
    agent_config_json: *const c_char,
) -> *mut pi_agent_handle {
    clear_last_error();
    match ffi_result(|| {
        let models = unsafe { required_ref(models, "models") }?;
        let config = unsafe { required_string(agent_config_json, "agent_config_json") }?;
        models
            .models
            .create_agent(config)
            .map(|agent| Box::into_raw(Box::new(pi_agent_handle { agent })))
            .map_err(|error| error.to_string())
    }) {
        Some(handle) => handle,
        None => ptr::null_mut(),
    }
}

/// Starts an agent run and returns its nonzero binding run identity.
///
/// The callback receives versioned sequenced envelopes on a binding-owned
/// worker thread. `user_data` is passed through unchanged and remains owned by
/// the host.
///
/// # Safety
///
/// `agent` must be live, `input_json` must be a readable NUL-terminated string,
/// and `user_data` must remain valid for callback use until the terminal
/// envelope is delivered.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pi_agent_run(
    agent: *mut pi_agent_handle,
    input_json: *const c_char,
    callback: pi_event_callback,
    user_data: *mut c_void,
) -> u64 {
    clear_last_error();
    ffi_result(|| {
        let agent = unsafe { required_ref(agent, "agent") }?;
        let input = unsafe { required_string(input_json, "input_json") }?;
        let callback = callback.ok_or_else(|| "callback must not be null".to_owned())?;
        agent
            .agent
            .start_callback(
                &input,
                RawEventCallback {
                    callback,
                    user_data: user_data as usize,
                },
            )
            .map_err(|error| error.to_string())
    })
    .unwrap_or_default()
}

/// Cancels the matching active run. A cancellation issued after
/// [`pi_agent_run`] returns but before `run_started` is delivered is retained
/// and applied as soon as the core run identity is published. Errors are
/// available through [`pi_last_error_message`].
///
/// # Safety
///
/// `agent` must be a live handle returned by [`pi_agent_create`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pi_agent_cancel(agent: *mut pi_agent_handle, run_id: u64) {
    clear_last_error();
    let _ = ffi_result(|| {
        let agent = unsafe { required_ref(agent, "agent") }?;
        agent
            .agent
            .cancel_run(run_id)
            .map_err(|error| error.to_string())
    });
}

/// Cancels and settles active work, then releases the opaque agent handle.
///
/// # Safety
///
/// `handle` must be null or a live handle returned by [`pi_agent_create`], and
/// it must be destroyed at most once. It must not be destroyed from its own
/// event callback.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pi_agent_destroy(handle: *mut pi_agent_handle) {
    clear_last_error();
    if handle.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| unsafe {
        drop(Box::from_raw(handle));
    }))
    .map_err(|_| set_last_error("panic while destroying agent handle"));
}

/// Begins provider login using the part 2 §6.5 challenge/response protocol.
/// Returns null on failure.
///
/// # Safety
///
/// `models` must be live, and every string pointer must address a readable
/// NUL-terminated string for the duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pi_auth_login_begin(
    models: *mut pi_models_handle,
    provider_id: *const c_char,
    auth_type: *const c_char,
    host_capabilities_json: *const c_char,
) -> *mut pi_auth_session {
    clear_last_error();
    match ffi_result(|| {
        let models = unsafe { required_ref(models, "models") }?;
        let provider_id = unsafe { required_string(provider_id, "provider_id") }?;
        let auth_type = unsafe { required_string(auth_type, "auth_type") }?;
        let capabilities =
            unsafe { required_string(host_capabilities_json, "host_capabilities_json") }?;
        models
            .models
            .auth_login_begin(provider_id, auth_type, capabilities)
            .map(|session| Box::into_raw(Box::new(pi_auth_session { session })))
            .map_err(|error| error.to_string())
    }) {
        Some(handle) => handle,
        None => ptr::null_mut(),
    }
}

/// Blocks until the next auth challenge, progress update, or terminal payload.
/// Returns a `PI_STATUS_*` value and fills `out_challenge->json` on every
/// successful receive, including terminal receives.
///
/// # Safety
///
/// `session` must be live and `out_challenge` must point to writable storage
/// whose `json` member is null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pi_auth_session_next(
    session: *mut pi_auth_session,
    out_challenge: *mut pi_auth_challenge,
) -> i32 {
    clear_last_error();
    ffi_result(|| {
        let session = unsafe { required_ref(session, "session") }?;
        let output = unsafe {
            out_challenge
                .as_mut()
                .ok_or_else(|| "out_challenge must not be null".to_owned())?
        };
        if !output.json.is_null() {
            return Err("out_challenge->json must be null before next".into());
        }
        let event = session.session.next().map_err(|error| error.to_string())?;
        output.json = into_c_string(event.json)?;
        Ok(match event.status {
            PiAuthSessionEventStatus::Challenge => PI_STATUS_OK,
            PiAuthSessionEventStatus::Completed => PI_STATUS_COMPLETE,
            PiAuthSessionEventStatus::Failed => PI_STATUS_ERROR,
            PiAuthSessionEventStatus::Cancelled => PI_STATUS_CANCELLED,
        })
    })
    .unwrap_or_else(classify_last_error)
}

/// Responds to an open auth challenge. A late losing response returns
/// [`PI_AUTH_CHALLENGE_SUPERSEDED`].
///
/// # Safety
///
/// `session` must be live, and both string pointers must address readable
/// NUL-terminated strings for the duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pi_auth_session_respond(
    session: *mut pi_auth_session,
    challenge_id: *const c_char,
    response_json: *const c_char,
) -> i32 {
    clear_last_error();
    ffi_result(|| {
        let session = unsafe { required_ref(session, "session") }?;
        let challenge_id = unsafe { required_string(challenge_id, "challenge_id") }?;
        let response_json = unsafe { required_string(response_json, "response_json") }?;
        match session
            .session
            .respond(challenge_id, response_json)
            .map_err(|error| error.to_string())?
        {
            PiAuthResponseStatus::Accepted => Ok(PI_STATUS_OK),
            PiAuthResponseStatus::ChallengeSuperseded => Ok(PI_AUTH_CHALLENGE_SUPERSEDED),
        }
    })
    .unwrap_or_else(classify_last_error)
}

/// Cancels the auth flow and all pending challenge paths.
///
/// # Safety
///
/// `session` must be a live handle returned by [`pi_auth_login_begin`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pi_auth_session_cancel(session: *mut pi_auth_session) {
    clear_last_error();
    let _ = ffi_result(|| {
        let session = unsafe { required_ref(session, "session") }?;
        session.session.cancel();
        Ok(())
    });
}

/// Releases a JSON string produced in [`pi_auth_challenge`].
///
/// # Safety
///
/// `challenge` must be null or point to writable storage. Its `json` member
/// must be null or the unmodified pointer returned by [`pi_auth_session_next`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pi_auth_challenge_clear(challenge: *mut pi_auth_challenge) {
    clear_last_error();
    if challenge.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| unsafe {
        let challenge = &mut *challenge;
        if !challenge.json.is_null() {
            drop(CString::from_raw(challenge.json));
            challenge.json = ptr::null_mut();
        }
    }))
    .map_err(|_| set_last_error("panic while clearing auth challenge"));
}

/// Cancels and releases an opaque auth-session handle.
///
/// # Safety
///
/// `session` must be null or a live handle returned by
/// [`pi_auth_login_begin`], and it must be destroyed at most once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pi_auth_session_destroy(session: *mut pi_auth_session) {
    clear_last_error();
    if session.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| unsafe {
        drop(Box::from_raw(session));
    }))
    .map_err(|_| set_last_error("panic while destroying auth session"));
}

fn ffi_result<T>(operation: impl FnOnce() -> Result<T, String>) -> Option<T> {
    match catch_unwind(AssertUnwindSafe(operation)) {
        Ok(Ok(value)) => Some(value),
        Ok(Err(error)) => {
            set_last_error(&error);
            None
        }
        Err(_) => {
            set_last_error("binding operation panicked");
            None
        }
    }
}

unsafe fn required_ref<'a, T>(pointer: *mut T, name: &str) -> Result<&'a T, String> {
    unsafe { pointer.as_ref() }.ok_or_else(|| format!("{name} must not be null"))
}

unsafe fn required_string(pointer: *const c_char, name: &str) -> Result<String, String> {
    if pointer.is_null() {
        return Err(format!("{name} must not be null"));
    }
    unsafe { CStr::from_ptr(pointer) }
        .to_str()
        .map(str::to_owned)
        .map_err(|error| format!("{name} must be UTF-8: {error}"))
}

fn into_c_string(value: String) -> Result<*mut c_char, String> {
    CString::new(value)
        .map(CString::into_raw)
        .map_err(|_| "JSON output unexpectedly contained a NUL byte".into())
}

fn clear_last_error() {
    LAST_ERROR.with(|slot| *slot.borrow_mut() = None);
}

fn set_last_error(message: &str) {
    let sanitized = message.replace('\0', "�");
    LAST_ERROR.with(|slot| {
        *slot.borrow_mut() = CString::new(sanitized).ok();
    });
}

fn classify_last_error() -> i32 {
    let invalid = LAST_ERROR.with(|slot| {
        slot.borrow().as_ref().is_some_and(|message| {
            let message = message.to_string_lossy();
            message.contains("must not be null")
                || message.contains("must be null")
                || message.contains("must be UTF-8")
                || message.contains("invalid")
                || message.contains("unknown authentication challenge")
                || message.contains("does not match authentication challenge")
        })
    });
    if invalid {
        PI_STATUS_INVALID_ARGUMENT
    } else {
        PI_STATUS_ERROR
    }
}
