/*
 *  bugsee.h
 *  bugsee-ffi
 *
 *  Copyright © 2026 Bugsee. All rights reserved.
 *
 *  C ABI for the Bugsee Rust SDK. Link against libbugsee_ffi (static or dynamic).
 *  Every function is panic-safe: a Rust panic never crosses this boundary — it
 *  is contained and reported as BUGSEE_PANIC.
 */

#ifndef BUGSEE_H
#define BUGSEE_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Result of an FFI call. */
typedef enum {
    BUGSEE_OK = 0,
    BUGSEE_INVALID_ARGUMENT = 1,
    BUGSEE_INTERNAL_ERROR = 2,
    BUGSEE_PANIC = 3,
    BUGSEE_NOT_LAUNCHED = 4,
    BUGSEE_TIMEOUT = 5
} BugseeStatus;

/* Log levels (match bugsee_log's `level` argument). */
#define BUGSEE_LEVEL_ERROR   1
#define BUGSEE_LEVEL_WARNING 2
#define BUGSEE_LEVEL_INFO    3
#define BUGSEE_LEVEL_DEBUG   4
#define BUGSEE_LEVEL_VERBOSE 5

/* Lifecycle. */
BugseeStatus bugsee_launch(const char *app_token);
BugseeStatus bugsee_launch_with_endpoint(const char *app_token, const char *endpoint);
BugseeStatus bugsee_stop(void);
int32_t      bugsee_is_active(void);
BugseeStatus bugsee_pause(void);
BugseeStatus bugsee_resume(void);

/* Timeline. */
BugseeStatus bugsee_log(int32_t level, const char *message);
BugseeStatus bugsee_event(const char *name);
BugseeStatus bugsee_event_with_params(const char *name, const char *params_json);
BugseeStatus bugsee_trace(const char *name, const char *value_json);

/* Identity & attributes. */
BugseeStatus bugsee_set_email(const char *email);
BugseeStatus bugsee_set_attribute(const char *key, const char *value_json);

/* Reporting. */
BugseeStatus bugsee_capture_exception(const char *name, const char *reason);
BugseeStatus bugsee_upload(void);
BugseeStatus bugsee_flush(uint32_t timeout_ms);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* BUGSEE_H */
