#ifndef PI_FFI_H
#define PI_FFI_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct pi_models_handle pi_models_handle;
typedef struct pi_agent_handle pi_agent_handle;
typedef struct pi_auth_session pi_auth_session;

typedef int32_t pi_status;

enum {
    PI_STATUS_OK = 0,
    PI_STATUS_COMPLETE = 1,
    PI_STATUS_CANCELLED = 2,
    PI_AUTH_CHALLENGE_SUPERSEDED = 3,
    PI_STATUS_ERROR = -1,
    PI_STATUS_INVALID_ARGUMENT = -2
};

typedef void (*pi_event_callback)(const char *envelope_json, void *user_data);

typedef struct pi_auth_challenge {
    char *json;
} pi_auth_challenge;

const char *pi_last_error_message(void);

pi_models_handle *pi_models_create(const char *config_json);
void pi_models_destroy(pi_models_handle *models);

pi_agent_handle *pi_agent_create(
    pi_models_handle *models,
    const char *agent_config_json
);

uint64_t pi_agent_run(
    pi_agent_handle *agent,
    const char *input_json,
    pi_event_callback callback,
    void *user_data
);

void pi_agent_cancel(pi_agent_handle *agent, uint64_t run_id);
void pi_agent_destroy(pi_agent_handle *agent);

pi_auth_session *pi_auth_login_begin(
    pi_models_handle *models,
    const char *provider_id,
    const char *auth_type,
    const char *host_capabilities_json
);

pi_status pi_auth_session_next(
    pi_auth_session *session,
    pi_auth_challenge *out_challenge
);

pi_status pi_auth_session_respond(
    pi_auth_session *session,
    const char *challenge_id,
    const char *response_json
);

void pi_auth_session_cancel(pi_auth_session *session);
void pi_auth_challenge_clear(pi_auth_challenge *challenge);
void pi_auth_session_destroy(pi_auth_session *session);

#ifdef __cplusplus
}
#endif

#endif
